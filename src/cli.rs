//! Command line interface.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use bson::{Bson, Document, oid::ObjectId};
use chrono::{DateTime, SecondsFormat, Utc};
use clap::{Args, Parser, Subcommand};
use tokio::net::TcpListener;
use tracing_subscriber::EnvFilter;

use crate::config::{Config, SelectionMode, ServerConfig};
use crate::duration::DurationSpec;
use crate::error::{AppError, Result};
use crate::maintenance;
use crate::model::{Account, Provider, TlsMode, parse_reset_at, validate_timezone};
use crate::render;
use crate::route::Route;
use crate::router::Router;
use crate::schedule::{Schedule, parse_window};
use crate::service;
use crate::smtp;
use crate::store::Store;

#[derive(Debug, Parser)]
#[command(
    name = "smtpmaster",
    version,
    about = "Route outbound SMTP through MongoDB-managed accounts",
    propagate_version = true
)]
pub struct Cli {
    /// Configuration file (default: ./smtpmaster.toml when present).
    #[arg(long, global = true, value_name = "PATH")]
    config: Option<PathBuf>,

    /// Print machine-readable JSON.
    #[arg(long, global = true)]
    json: bool,

    /// Increase log verbosity (-v, -vv).
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Create MongoDB indexes and verify connectivity.
    Init,
    /// Manage SMTP accounts.
    Account {
        #[command(subcommand)]
        command: AccountCommand,
    },
    /// Manage inbound identities and their outbound account pools.
    Route {
        #[command(subcommand)]
        command: RouteCommand,
    },
    /// Run the SMTP relay that distributes mail across accounts.
    Serve(ServeArgs),
    /// Reset quota windows and disable expired accounts.
    Maintenance(MaintenanceArgs),
    /// Show routing statistics.
    Stats,
    /// Enable/disable the token-protected web dashboard at runtime.
    Admin {
        #[command(subcommand)]
        command: AdminCommand,
    },
    /// Generate and install a systemd unit for auto-start.
    #[command(visible_alias = "autostart")]
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

#[derive(Debug, Subcommand)]
enum AccountCommand {
    /// Add an SMTP account.
    Add(AddArgs),
    /// List accounts.
    List {
        /// Include disabled accounts (they are listed by default).
        #[arg(long)]
        active_only: bool,
    },
    /// Show one account.
    Show { account: String },
    /// Change an existing account.
    Update(UpdateArgs),
    /// Delete an account permanently.
    Delete {
        account: String,
        /// Confirm the deletion.
        #[arg(long)]
        yes: bool,
    },
    /// Enable an account (clears failure counters).
    Enable { account: String },
    /// Disable an account without deleting it.
    Disable {
        account: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Send a test email through one account.
    Test {
        account: String,
        #[arg(long)]
        to: String,
    },
    /// Reset an account's quota counter immediately.
    Reset { account: String },
}

#[derive(Debug, Args)]
struct AddArgs {
    /// Unique account name.
    #[arg(long)]
    name: String,
    /// google (smtp.gmail.com preset with app password) or smtp (custom relay).
    #[arg(long, default_value = "smtp")]
    provider: Provider,
    /// SMTP server host (required for --provider smtp).
    #[arg(long)]
    host: Option<String>,
    /// SMTP server port (default: 587, or 465 with --tls implicit).
    #[arg(long)]
    port: Option<u16>,
    /// TLS mode: starttls, implicit or none.
    #[arg(long)]
    tls: Option<TlsMode>,
    #[arg(long)]
    username: String,
    /// Password (prefer --password-env or --password-stdin).
    #[arg(long)]
    password: Option<String>,
    /// Read the password from stdin.
    #[arg(long)]
    password_stdin: bool,
    /// Read the password from an environment variable.
    #[arg(long, value_name = "VAR")]
    password_env: Option<String>,
    /// Envelope/From address (defaults to the username when it is an address).
    #[arg(long)]
    from: Option<String>,
    /// Display name used when rewriting the From header.
    #[arg(long)]
    from_name: Option<String>,
    /// Lower is preferred.
    #[arg(long, default_value_t = 100)]
    priority: i32,
    /// Sends per quota window (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    count_max: i64,
    /// Sends per minute (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    rate_limit: i64,
    /// Lifetime send cap for the purchased package (0 = unlimited).
    #[arg(long, default_value_t = 0)]
    total_max: i64,
    /// Daily reset time in the account timezone, HH:MM.
    #[arg(long, default_value = "00:00")]
    reset_at: String,
    /// IANA timezone for the reset time and windows.
    #[arg(long, default_value = "UTC")]
    timezone: String,
    /// Expire after a duration: 15d, 3m (months), 2y, 36h.
    #[arg(long, conflicts_with = "expires_at")]
    expires_in: Option<String>,
    /// Expire at an RFC3339 timestamp.
    #[arg(long, conflicts_with = "expires_in")]
    expires_at: Option<String>,
    /// Usage window: `[days] HH:MM-HH:MM[@priority]`, repeatable.
    #[arg(long = "window", value_name = "SPEC")]
    windows: Vec<String>,
    /// Create the account disabled.
    #[arg(long)]
    inactive: bool,
}

#[derive(Debug, Args)]
struct UpdateArgs {
    account: String,
    /// Rename the account.
    #[arg(long)]
    rename: Option<String>,
    #[arg(long)]
    host: Option<String>,
    #[arg(long)]
    port: Option<u16>,
    #[arg(long)]
    tls: Option<TlsMode>,
    #[arg(long)]
    username: Option<String>,
    #[arg(long)]
    password: Option<String>,
    #[arg(long)]
    password_stdin: bool,
    #[arg(long, value_name = "VAR")]
    password_env: Option<String>,
    #[arg(long)]
    from: Option<String>,
    #[arg(long)]
    from_name: Option<String>,
    #[arg(long)]
    priority: Option<i32>,
    #[arg(long)]
    count_max: Option<i64>,
    #[arg(long)]
    rate_limit: Option<i64>,
    #[arg(long)]
    total_max: Option<i64>,
    #[arg(long)]
    reset_at: Option<String>,
    #[arg(long)]
    timezone: Option<String>,
    #[arg(long, conflicts_with = "expires_at")]
    expires_in: Option<String>,
    #[arg(long, conflicts_with = "expires_in")]
    expires_at: Option<String>,
    /// Remove the expiration date.
    #[arg(long)]
    no_expiry: bool,
    /// Replace the usage windows, repeatable.
    #[arg(long = "window", value_name = "SPEC")]
    windows: Vec<String>,
    /// Remove all usage windows.
    #[arg(long)]
    clear_windows: bool,
    /// Enable the account.
    #[arg(long, conflicts_with = "disable")]
    enable: bool,
    /// Disable the account.
    #[arg(long, conflicts_with = "enable")]
    disable: bool,
}

#[derive(Debug, Subcommand)]
enum RouteCommand {
    /// Add an inbound route with its own outbound account pool.
    Add(RouteAddArgs),
    /// List routes.
    List,
    /// Show one route.
    Show { route: String },
    /// Change a route.
    Update(RouteUpdateArgs),
    /// Delete a route permanently.
    Delete {
        route: String,
        /// Confirm the deletion.
        #[arg(long)]
        yes: bool,
    },
    /// Enable a route.
    Enable { route: String },
    /// Disable a route.
    Disable { route: String },
}

#[derive(Debug, Args)]
struct RouteAddArgs {
    /// Unique route name.
    #[arg(long)]
    name: String,
    /// Inbound SMTP AUTH username used by the external app.
    #[arg(long)]
    username: String,
    #[arg(long)]
    password: Option<String>,
    #[arg(long)]
    password_stdin: bool,
    #[arg(long, value_name = "VAR")]
    password_env: Option<String>,
    /// Outbound account (name or id), repeatable; at least one.
    #[arg(long = "account", value_name = "ACCOUNT", required = true)]
    accounts: Vec<String>,
    #[arg(long)]
    description: Option<String>,
    /// Create the route disabled.
    #[arg(long)]
    inactive: bool,
}

#[derive(Debug, Args)]
struct RouteUpdateArgs {
    route: String,
    #[arg(long)]
    rename: Option<String>,
    #[arg(long)]
    username: Option<String>,
    #[arg(long)]
    password: Option<String>,
    #[arg(long)]
    password_stdin: bool,
    #[arg(long, value_name = "VAR")]
    password_env: Option<String>,
    /// Replace the outbound account pool (repeatable).
    #[arg(long = "account", value_name = "ACCOUNT")]
    accounts: Vec<String>,
    /// Remove every outbound account from the pool.
    #[arg(long, conflicts_with = "accounts")]
    clear_accounts: bool,
    #[arg(long)]
    description: Option<String>,
    #[arg(long, conflicts_with = "disable")]
    enable: bool,
    #[arg(long, conflicts_with = "enable")]
    disable: bool,
}

#[derive(Debug, Subcommand)]
enum AdminCommand {
    /// Enable the web dashboard (applies without restarting serve).
    Enable {
        /// Dashboard token (at least 16 characters). Reused when omitted.
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        /// Listen address, e.g. 127.0.0.1:8787.
        #[arg(long, value_name = "ADDR")]
        listen: Option<String>,
    },
    /// Disable the web dashboard.
    Disable,
    /// Show the dashboard settings.
    Status,
}

#[derive(Debug, Args)]
struct ServeArgs {
    #[arg(long)]
    listen: Option<String>,
    #[arg(long)]
    mode: Option<SelectionMode>,
    #[arg(long)]
    concurrency: Option<usize>,
    /// Send one message at a time.
    #[arg(long)]
    sequential: bool,
    #[arg(long)]
    auth_required: bool,
    #[arg(long)]
    rewrite_from_header: bool,
    #[arg(long)]
    max_message_size: Option<usize>,
    #[arg(long)]
    max_connections: Option<usize>,
    /// Detach and keep running in the background.
    #[arg(long)]
    daemon: bool,
    /// PID file used by --daemon (default: state directory).
    #[arg(long, value_name = "PATH")]
    pid_file: Option<PathBuf>,
    /// Log file used by --daemon (default: state directory).
    #[arg(long, value_name = "PATH")]
    log_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct ServiceOptions {
    /// Install a systemd *user* unit (no root required).
    #[arg(long, conflicts_with = "system")]
    user: bool,
    /// Install a system unit (default; needs root).
    #[arg(long, conflicts_with = "user")]
    system: bool,
    /// Unit name without the .service suffix.
    #[arg(long, default_value = "smtpmaster")]
    name: String,
    /// Executable to run (default: the current binary).
    #[arg(long)]
    bin: Option<PathBuf>,
    /// Environment variable for the service, KEY=VALUE (repeatable).
    #[arg(long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// Show what would be done without touching the system.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Debug, Subcommand)]
enum ServiceCommand {
    /// Print the generated unit file.
    Print(ServiceOptions),
    /// Write the unit, reload systemd and enable the service.
    Install {
        #[command(flatten)]
        options: ServiceOptions,
        /// Enable the unit without starting it now.
        #[arg(long)]
        no_start: bool,
        /// Overwrite an existing unit file.
        #[arg(long)]
        force: bool,
    },
    /// Stop, disable and remove the unit.
    Uninstall(ServiceOptions),
    /// Show the service status via systemctl.
    Status(ServiceOptions),
}

#[derive(Debug, Args)]
struct MaintenanceArgs {
    /// Keep running instead of doing a single pass.
    #[arg(long)]
    watch: bool,
    /// Seconds between passes in watch mode (default: config value).
    #[arg(long)]
    interval: Option<u64>,
}

pub async fn run() -> Result<()> {
    let cli = Cli::parse();
    init_tracing(cli.verbose);
    let config = Config::load(cli.config.as_deref())?;
    match cli.command {
        Command::Init => {
            let store = connect(&config).await?;
            store.ensure_indexes().await?;
            println!(
                "indexes ready in database `{}` at {}",
                config.mongodb.database, config.mongodb.uri
            );
            Ok(())
        }
        Command::Account { command } => handle_account(&config, command, cli.json).await,
        Command::Route { command } => handle_route(&config, command, cli.json).await,
        Command::Admin { command } => handle_admin(&config, command, cli.json).await,
        Command::Serve(args) => serve(config, args).await,
        Command::Maintenance(args) => {
            let store = Arc::new(connect(&config).await?);
            store.ensure_indexes().await?;
            if args.watch {
                let interval = args
                    .interval
                    .unwrap_or(config.maintenance.interval_secs)
                    .max(1);
                maintenance::watch(store, Duration::from_secs(interval)).await;
                Ok(())
            } else {
                let report = store.maintenance(Utc::now()).await?;
                if cli.json {
                    print_json(&report)
                } else {
                    println!(
                        "windows reset: {}\naccounts disabled: {}",
                        report.windows_reset, report.accounts_disabled
                    );
                    Ok(())
                }
            }
        }
        Command::Service { command } => handle_service(cli.config.as_deref(), command),
        Command::Stats => {
            let store = connect(&config).await?;
            let stats = store.stats(Utc::now()).await?;
            if cli.json {
                print_json(&stats)
            } else {
                let rows = vec![
                    vec!["accounts".to_string(), stats.total_accounts.to_string()],
                    vec!["active".to_string(), stats.active_accounts.to_string()],
                    vec!["expired".to_string(), stats.expired_accounts.to_string()],
                    vec!["disabled".to_string(), stats.disabled_accounts.to_string()],
                    vec![
                        "capped quota used / capacity".to_string(),
                        format!(
                            "{} / {}",
                            stats.used_current_window, stats.capacity_current_window
                        ),
                    ],
                    vec![
                        "unlimited accounts".to_string(),
                        stats.unlimited_accounts.to_string(),
                    ],
                    vec!["sent (24h)".to_string(), stats.sent_24h.to_string()],
                    vec!["failed (24h)".to_string(), stats.failed_24h.to_string()],
                ];
                render::table(&["METRIC", "VALUE"], &rows);
                Ok(())
            }
        }
    }
}

async fn handle_account(config: &Config, command: AccountCommand, json: bool) -> Result<()> {
    let store = connect(config).await?;
    match command {
        AccountCommand::Add(args) => {
            store.ensure_indexes().await?;
            let now = Utc::now();
            let password =
                resolve_password(&args.password, args.password_stdin, &args.password_env)?
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        AppError::Invalid(
                            "provide --password, --password-stdin or --password-env".into(),
                        )
                    })?;
            if args.username.trim().is_empty() {
                return Err(AppError::Invalid("--username must not be empty".into()));
            }
            if args.count_max < 0 {
                return Err(AppError::Invalid("--count-max cannot be negative".into()));
            }
            if args.rate_limit < 0 {
                return Err(AppError::Invalid("--rate-limit cannot be negative".into()));
            }
            if args.total_max < 0 {
                return Err(AppError::Invalid("--total-max cannot be negative".into()));
            }
            let from = match args.from {
                Some(from) => from,
                None if args.username.contains('@') => args.username.clone(),
                None => {
                    return Err(AppError::Invalid(
                        "--from is required when --username is not an email address".into(),
                    ));
                }
            };
            if !from.contains('@') {
                return Err(AppError::Invalid(format!(
                    "`{from}` is not an email address"
                )));
            }
            let mut account =
                Account::new(args.name, args.provider, args.username, password, from, now);
            match args.host {
                Some(host) => account.host = host,
                None if args.provider == Provider::Custom => {
                    return Err(AppError::Invalid(
                        "--host is required for --provider smtp".into(),
                    ));
                }
                None => {}
            }
            let tls = args.tls.unwrap_or({
                if args.port == Some(465) {
                    TlsMode::Implicit
                } else {
                    TlsMode::Starttls
                }
            });
            account.tls = tls;
            account.port = args
                .port
                .unwrap_or(if tls == TlsMode::Implicit { 465 } else { 587 });
            account.from_name = args.from_name.filter(|name| !name.trim().is_empty());
            account.priority = args.priority;
            account.count_max = args.count_max;
            account.rate_limit_per_min = args.rate_limit;
            account.rate_tokens = args.rate_limit as f64;
            account.total_max = args.total_max;
            account.reset_at = normalize_reset_at(&args.reset_at)?;
            validate_timezone(&args.timezone)?;
            account.timezone = args.timezone;
            account.expires_at =
                expiry_from_flags(args.expires_in.as_deref(), args.expires_at.as_deref(), now)?;
            if !args.windows.is_empty() {
                let windows = args
                    .windows
                    .iter()
                    .map(|spec| parse_window(spec))
                    .collect::<Result<Vec<_>>>()?;
                account.schedule = Some(Schedule { windows });
            }
            if args.inactive {
                account.active = false;
                account.disabled_reason = Some("created inactive".to_string());
            }
            account.updated_at = now;
            store.insert_account(&account).await?;

            if json {
                print_json(&account.redacted())
            } else {
                println!(
                    "account `{}` created ({}), quota {}, rate {}",
                    account.name,
                    account.endpoint_label(),
                    account.quota_label(),
                    account.rate_label()
                );
                Ok(())
            }
        }
        AccountCommand::List { active_only } => {
            let accounts = store.list_accounts(active_only).await?;
            if json {
                let redacted: Vec<Account> = accounts.iter().map(Account::redacted).collect();
                print_json(&redacted)
            } else if accounts.is_empty() {
                println!("no accounts");
                Ok(())
            } else {
                let now = Utc::now();
                let headers = render::account_headers();
                let rows: Vec<Vec<String>> = accounts
                    .iter()
                    .map(|account| render::account_row(account, now))
                    .collect();
                render::table(&headers, &rows);
                Ok(())
            }
        }
        AccountCommand::Show { account } => {
            let account = store.find_account(&account).await?;
            if json {
                return print_json(&account.redacted());
            }
            let now = Utc::now();
            let rows = vec![
                kv("name", account.name.clone()),
                kv("id", account.id.to_hex()),
                kv("provider", account.provider.to_string()),
                kv("status", account.status_label(now)),
                kv("endpoint", account.endpoint_label()),
                kv("username", account.username.clone()),
                kv("password", "***".to_string()),
                kv("from", from_label(&account)),
                kv("quota", account.quota_label()),
                kv("total quota", account.total_label()),
                kv("rate", account.rate_label()),
                kv(
                    "reset",
                    format!("{} {}", account.reset_at, account.timezone),
                ),
                kv("expires", account.expiry_label()),
                kv("windows", schedule_label(&account.schedule)),
                kv("total sent", account.total_sent.to_string()),
                kv(
                    "last used",
                    account
                        .last_used_at
                        .map(render::format_timestamp)
                        .unwrap_or_else(|| "-".to_string()),
                ),
                kv(
                    "last success",
                    account
                        .last_success_at
                        .map(render::format_timestamp)
                        .unwrap_or_else(|| "-".to_string()),
                ),
                kv("failures", account.consecutive_failures.to_string()),
                kv("last error", account.last_error.clone().unwrap_or_default()),
                kv(
                    "created",
                    account
                        .created_at
                        .to_rfc3339_opts(SecondsFormat::Secs, true),
                ),
                kv(
                    "updated",
                    account
                        .updated_at
                        .to_rfc3339_opts(SecondsFormat::Secs, true),
                ),
            ];
            for (key, value) in rows {
                println!("{key:<14}{value}");
            }
            Ok(())
        }
        AccountCommand::Update(args) => {
            let account = store.find_account(&args.account).await?;
            let now = Utc::now();
            let mut set = Document::new();
            let mut unset = Document::new();

            if let Some(rename) = args.rename {
                if rename.trim().is_empty() {
                    return Err(AppError::Invalid("--rename must not be empty".into()));
                }
                set.insert("name", rename);
            }
            if let Some(host) = args.host {
                set.insert("host", host);
            }
            if let Some(port) = args.port {
                set.insert("port", i32::from(port));
            }
            if let Some(tls) = args.tls {
                set.insert("tls", tls.as_str());
            }
            if let Some(username) = args.username {
                if username.trim().is_empty() {
                    return Err(AppError::Invalid("--username must not be empty".into()));
                }
                set.insert("username", username);
            }
            if let Some(password) =
                resolve_password(&args.password, args.password_stdin, &args.password_env)?
            {
                if password.is_empty() {
                    return Err(AppError::Invalid("password must not be empty".into()));
                }
                set.insert("password", password);
            }
            if let Some(from) = args.from {
                if !from.contains('@') {
                    return Err(AppError::Invalid(format!(
                        "`{from}` is not an email address"
                    )));
                }
                set.insert("from_address", from);
            }
            match args.from_name {
                Some(name) if name.trim().is_empty() => {
                    unset.insert("from_name", "");
                }
                Some(name) => {
                    set.insert("from_name", name);
                }
                None => {}
            }
            if let Some(priority) = args.priority {
                set.insert("priority", priority);
            }
            if let Some(count_max) = args.count_max {
                if count_max < 0 {
                    return Err(AppError::Invalid("--count-max cannot be negative".into()));
                }
                set.insert("count_max", count_max);
            }
            if let Some(rate_limit) = args.rate_limit {
                if rate_limit < 0 {
                    return Err(AppError::Invalid("--rate-limit cannot be negative".into()));
                }
                set.insert("rate_limit_per_min", rate_limit);
                set.insert("rate_tokens", rate_limit as f64);
                set.insert("rate_updated_at", bdate(now));
            }
            if let Some(total_max) = args.total_max {
                if total_max < 0 {
                    return Err(AppError::Invalid("--total-max cannot be negative".into()));
                }
                set.insert("total_max", total_max);
            }
            if let Some(reset_at) = args.reset_at {
                set.insert("reset_at", normalize_reset_at(&reset_at)?);
            }
            if let Some(timezone) = args.timezone {
                validate_timezone(&timezone)?;
                set.insert("timezone", timezone);
            }
            if args.no_expiry && (args.expires_in.is_some() || args.expires_at.is_some()) {
                return Err(AppError::Invalid(
                    "--no-expiry conflicts with --expires-in/--expires-at".into(),
                ));
            }
            if args.no_expiry {
                unset.insert("expires_at", "");
            } else if let Some(expires_at) =
                expiry_from_flags(args.expires_in.as_deref(), args.expires_at.as_deref(), now)?
            {
                set.insert("expires_at", bdate(expires_at));
            }
            if args.clear_windows && !args.windows.is_empty() {
                return Err(AppError::Invalid(
                    "--clear-windows conflicts with --window".into(),
                ));
            }
            if args.clear_windows {
                unset.insert("schedule", "");
            } else if !args.windows.is_empty() {
                let windows = args
                    .windows
                    .iter()
                    .map(|spec| parse_window(spec))
                    .collect::<Result<Vec<_>>>()?;
                let schedule = Schedule { windows };
                set.insert("schedule", bson_of(&schedule)?);
            }
            if args.enable {
                set.insert("active", true);
                set.insert("disabled_reason", Bson::Null);
                set.insert("consecutive_failures", 0i64);
                set.insert("last_error", Bson::Null);
            }
            if args.disable {
                set.insert("active", false);
                set.insert(
                    "disabled_reason",
                    format!(
                        "disabled manually at {}",
                        now.to_rfc3339_opts(SecondsFormat::Secs, true)
                    ),
                );
            }
            if set.is_empty() && unset.is_empty() {
                return Err(AppError::Invalid("nothing to update".into()));
            }
            set.insert("updated_at", bdate(now));

            let mut update = Document::new();
            update.insert("$set", set);
            if !unset.is_empty() {
                update.insert("$unset", unset);
            }
            let updated = store.update_account(account.id, update).await?;
            if json {
                print_json(&updated.redacted())
            } else {
                println!("account `{}` updated", updated.name);
                Ok(())
            }
        }
        AccountCommand::Delete { account, yes } => {
            if !yes {
                return Err(AppError::Invalid("refusing to delete without --yes".into()));
            }
            let account = store.find_account(&account).await?;
            store.delete_account(account.id).await?;
            println!("account `{}` deleted", account.name);
            Ok(())
        }
        AccountCommand::Enable { account } => {
            let account = store.find_account(&account).await?;
            let updated = store.set_active(account.id, true, None).await?;
            if json {
                print_json(&updated.redacted())
            } else {
                println!("account `{}` enabled", updated.name);
                Ok(())
            }
        }
        AccountCommand::Disable { account, reason } => {
            let account = store.find_account(&account).await?;
            let reason = reason.unwrap_or_else(|| "disabled manually".to_string());
            let updated = store.set_active(account.id, false, Some(&reason)).await?;
            if json {
                print_json(&updated.redacted())
            } else {
                println!("account `{}` disabled: {reason}", updated.name);
                Ok(())
            }
        }
        AccountCommand::Test { account, to } => {
            let account = store.find_account(&account).await?;
            if !to.contains('@') {
                return Err(AppError::Invalid(format!("`{to}` is not an email address")));
            }
            let message = test_message(&account, &to);
            smtp::send(
                &account,
                &account.from_address,
                std::slice::from_ref(&to),
                &message,
                &config.server.hostname,
                Duration::from_secs(config.server.send_timeout_secs.max(1)),
            )
            .await?;
            println!(
                "test message delivered to {to} through `{}` ({})",
                account.name,
                account.endpoint_label()
            );
            Ok(())
        }
        AccountCommand::Reset { account } => {
            let account = store.find_account(&account).await?;
            let updated = store.reset_usage(&account, Utc::now()).await?;
            if json {
                print_json(&updated.redacted())
            } else {
                println!(
                    "quota reset for `{}`: {}",
                    updated.name,
                    updated.quota_label()
                );
                Ok(())
            }
        }
    }
}

async fn serve(config: Config, args: ServeArgs) -> Result<()> {
    if args.daemon && std::env::var("SMTPMASTER_DAEMONIZED").as_deref() != Ok("1") {
        return daemonize(&args);
    }
    let mut server: ServerConfig = config.server.clone();
    if let Some(listen) = args.listen {
        server.listen = listen;
    }
    if let Some(mode) = args.mode {
        server.mode = mode;
    }
    if let Some(concurrency) = args.concurrency {
        server.concurrency = concurrency;
    }
    if args.sequential {
        server.sequential = true;
    }
    if args.auth_required {
        server.auth_required = true;
    }
    if args.rewrite_from_header {
        server.rewrite_from_header = true;
    }
    if let Some(size) = args.max_message_size {
        server.max_message_size = size;
    }
    if let Some(connections) = args.max_connections {
        server.max_connections = connections;
    }
    let store = Arc::new(connect(&config).await?);
    store.ensure_indexes().await?;
    if server.auth_required && server.users.is_empty() && !store.routes_active().await? {
        return Err(AppError::Config(
            "authentication is required but neither routes nor server.users are configured".into(),
        ));
    }
    let router = Arc::new(Router::new(Arc::clone(&store), &server));
    let listener = TcpListener::bind(&server.listen).await?;
    let address = listener.local_addr()?;
    tracing::info!(
        %address,
        mode = %server.mode,
        workers = if server.sequential { 1 } else { server.concurrency },
        "smtp router listening"
    );

    let interval = Duration::from_secs(config.maintenance.interval_secs.max(1));
    tokio::spawn(maintenance::watch(Arc::clone(&store), interval));
    // Toggled from the CLI through MongoDB; never restarts the SMTP side.
    tokio::spawn(crate::admin::watch(
        Arc::clone(&store),
        Duration::from_secs(2),
    ));
    tokio::select! {
        result = crate::smtp::server::serve(listener, router, Arc::clone(&store), server) => result,
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("shutdown signal received");
            Ok(())
        }
    }
}

async fn connect(config: &Config) -> Result<Store> {
    Store::connect(
        &config.mongodb.uri,
        &config.mongodb.database,
        config.log.retention_days,
    )
    .await
}

async fn handle_admin(config: &Config, command: AdminCommand, json: bool) -> Result<()> {
    let store = connect(config).await?;
    match command {
        AdminCommand::Enable { token, listen } => {
            let mut settings = store.admin_settings().await?;
            if let Some(token) = token {
                let token = token.trim().to_string();
                if token.len() < 16 {
                    return Err(AppError::Invalid(
                        "the dashboard token must be at least 16 characters".into(),
                    ));
                }
                settings.token = token;
            }
            if settings.token.is_empty() {
                return Err(AppError::Invalid(
                    "provide --token (or enable the dashboard once with a token)".into(),
                ));
            }
            if let Some(listen) = listen {
                let address: std::net::SocketAddr = listen.parse().map_err(|_| {
                    AppError::Invalid(format!(
                        "`{listen}` is not a valid listen address (e.g. 127.0.0.1:8787)"
                    ))
                })?;
                if !address.ip().is_loopback() {
                    eprintln!(
                        "warning: the dashboard will be reachable from the network on {address}; keep the token secret"
                    );
                }
                settings.listen = listen;
            }
            settings.enabled = true;
            settings.updated_at = Utc::now();
            store.save_admin_settings(&settings).await?;
            if json {
                print_json(&serde_json::json!({
                    "enabled": true,
                    "listen": settings.listen,
                    "token": settings.token_masked(),
                }))
            } else {
                println!(
                    "admin dashboard enabled at http://{} (token {})",
                    settings.listen,
                    settings.token_masked()
                );
                println!("the running `serve` process picks this up within a few seconds");
                Ok(())
            }
        }
        AdminCommand::Disable => {
            let mut settings = store.admin_settings().await?;
            settings.enabled = false;
            settings.updated_at = Utc::now();
            store.save_admin_settings(&settings).await?;
            if json {
                print_json(&serde_json::json!({
                    "enabled": false,
                    "listen": settings.listen,
                    "token": settings.token_masked(),
                }))
            } else {
                println!("admin dashboard disabled");
                Ok(())
            }
        }
        AdminCommand::Status => {
            let settings = store.admin_settings().await?;
            if json {
                print_json(&serde_json::json!({
                    "enabled": settings.enabled,
                    "listen": settings.listen,
                    "token": settings.token_masked(),
                    "updated_at": settings.updated_at.to_rfc3339_opts(SecondsFormat::Secs, true),
                }))
            } else {
                println!("enabled   {}", settings.enabled);
                println!("listen    {}", settings.listen);
                println!("token     {}", settings.token_masked());
                println!(
                    "updated   {}",
                    settings
                        .updated_at
                        .to_rfc3339_opts(SecondsFormat::Secs, true)
                );
                Ok(())
            }
        }
    }
}

async fn handle_route(config: &Config, command: RouteCommand, json: bool) -> Result<()> {
    let store = connect(config).await?;
    match command {
        RouteCommand::Add(args) => {
            store.ensure_indexes().await?;
            let password =
                resolve_password(&args.password, args.password_stdin, &args.password_env)?
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        AppError::Invalid(
                            "provide --password, --password-stdin or --password-env".into(),
                        )
                    })?;
            if args.username.trim().is_empty() {
                return Err(AppError::Invalid("--username must not be empty".into()));
            }
            let account_ids = resolve_account_ids(&store, &args.accounts).await?;
            if account_ids.is_empty() {
                return Err(AppError::Invalid(
                    "a route needs at least one account".into(),
                ));
            }
            let now = Utc::now();
            let mut route = Route::new(args.name, args.username, password, account_ids, now);
            route.description = args.description.filter(|value| !value.trim().is_empty());
            if args.inactive {
                route.active = false;
            }
            store.insert_route(&route).await?;
            if json {
                print_json(&route.redacted())
            } else {
                println!(
                    "route `{}` created for inbound user `{}` with {} account(s)",
                    route.name,
                    route.username,
                    route.account_ids.len()
                );
                Ok(())
            }
        }
        RouteCommand::List => {
            let routes = store.list_routes().await?;
            if json {
                let redacted: Vec<Route> = routes.iter().map(Route::redacted).collect();
                return print_json(&redacted);
            }
            if routes.is_empty() {
                println!("no routes");
                return Ok(());
            }
            let names = account_names(&store).await?;
            let headers = vec![
                "NAME",
                "USERNAME",
                "STATUS",
                "ACCOUNTS",
                "DESCRIPTION",
                "UPDATED",
            ];
            let rows: Vec<Vec<String>> = routes
                .iter()
                .map(|route| {
                    vec![
                        route.name.clone(),
                        route.username.clone(),
                        route.status_label().to_string(),
                        route_accounts_label(route, &names),
                        truncate_text(route.description.clone().unwrap_or_default(), 30),
                        route.updated_at.format("%Y-%m-%d %H:%M").to_string(),
                    ]
                })
                .collect();
            render::table(&headers, &rows);
            Ok(())
        }
        RouteCommand::Show { route } => {
            let route = store.find_route(&route).await?;
            if json {
                return print_json(&route.redacted());
            }
            let names = account_names(&store).await?;
            let accounts = if route.account_ids.is_empty() {
                "-".to_string()
            } else {
                route
                    .account_ids
                    .iter()
                    .map(|id| {
                        names
                            .get(id)
                            .cloned()
                            .unwrap_or_else(|| format!("missing:{}", id.to_hex()))
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            for (key, value) in [
                kv("name", route.name.clone()),
                kv("id", route.id.to_hex()),
                kv("username", route.username.clone()),
                kv("password", "***".to_string()),
                kv("status", route.status_label().to_string()),
                kv("description", route.description.clone().unwrap_or_default()),
                kv("accounts", accounts),
                kv(
                    "created",
                    route.created_at.to_rfc3339_opts(SecondsFormat::Secs, true),
                ),
                kv(
                    "updated",
                    route.updated_at.to_rfc3339_opts(SecondsFormat::Secs, true),
                ),
            ] {
                println!("{key:<14}{value}");
            }
            Ok(())
        }
        RouteCommand::Update(args) => {
            let route = store.find_route(&args.route).await?;
            let now = Utc::now();
            let mut set = Document::new();
            let mut unset = Document::new();
            if let Some(rename) = args.rename {
                if rename.trim().is_empty() {
                    return Err(AppError::Invalid("--rename must not be empty".into()));
                }
                set.insert("name", rename);
            }
            if let Some(username) = args.username {
                if username.trim().is_empty() {
                    return Err(AppError::Invalid("--username must not be empty".into()));
                }
                set.insert("username", username);
            }
            if let Some(password) =
                resolve_password(&args.password, args.password_stdin, &args.password_env)?
            {
                if password.is_empty() {
                    return Err(AppError::Invalid("password must not be empty".into()));
                }
                set.insert("password", password);
            }
            if let Some(description) = args.description {
                if description.trim().is_empty() {
                    unset.insert("description", "");
                } else {
                    set.insert("description", description);
                }
            }
            if args.clear_accounts {
                set.insert("account_ids", Vec::<ObjectId>::new());
            } else if !args.accounts.is_empty() {
                let ids = resolve_account_ids(&store, &args.accounts).await?;
                if ids.is_empty() {
                    return Err(AppError::Invalid(
                        "a route needs at least one account".into(),
                    ));
                }
                set.insert("account_ids", ids);
            }
            if args.enable {
                set.insert("active", true);
            }
            if args.disable {
                set.insert("active", false);
            }
            if set.is_empty() && unset.is_empty() {
                return Err(AppError::Invalid("nothing to update".into()));
            }
            set.insert("updated_at", bdate(now));
            let mut update = Document::new();
            update.insert("$set", set);
            if !unset.is_empty() {
                update.insert("$unset", unset);
            }
            let updated = store.update_route(route.id, update).await?;
            if json {
                print_json(&updated.redacted())
            } else {
                println!("route `{}` updated", updated.name);
                Ok(())
            }
        }
        RouteCommand::Delete { route, yes } => {
            if !yes {
                return Err(AppError::Invalid("refusing to delete without --yes".into()));
            }
            let route = store.find_route(&route).await?;
            store.delete_route(route.id).await?;
            println!("route `{}` deleted", route.name);
            Ok(())
        }
        RouteCommand::Enable { route } => {
            let route = store.find_route(&route).await?;
            let updated = store.set_route_active(route.id, true).await?;
            if json {
                print_json(&updated.redacted())
            } else {
                println!("route `{}` enabled", updated.name);
                Ok(())
            }
        }
        RouteCommand::Disable { route } => {
            let route = store.find_route(&route).await?;
            let updated = store.set_route_active(route.id, false).await?;
            if json {
                print_json(&updated.redacted())
            } else {
                println!("route `{}` disabled", updated.name);
                Ok(())
            }
        }
    }
}

async fn resolve_account_ids(store: &Store, selectors: &[String]) -> Result<Vec<ObjectId>> {
    let mut ids = Vec::new();
    for selector in selectors {
        let account = store.find_account(selector).await?;
        if !ids.contains(&account.id) {
            ids.push(account.id);
        }
    }
    Ok(ids)
}

async fn account_names(store: &Store) -> Result<HashMap<ObjectId, String>> {
    Ok(store
        .list_accounts(false)
        .await?
        .into_iter()
        .map(|account| (account.id, account.name))
        .collect())
}

fn route_accounts_label(route: &Route, names: &HashMap<ObjectId, String>) -> String {
    if route.account_ids.is_empty() {
        return "-".to_string();
    }
    truncate_text(
        route
            .account_ids
            .iter()
            .map(|id| {
                names
                    .get(id)
                    .cloned()
                    .unwrap_or_else(|| format!("missing:{}", id.to_hex()))
            })
            .collect::<Vec<_>>()
            .join(","),
        40,
    )
}

fn truncate_text(input: String, max: usize) -> String {
    if input.chars().count() <= max {
        return input;
    }
    let mut out: String = input.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

fn handle_service(config_path: Option<&std::path::Path>, command: ServiceCommand) -> Result<()> {
    match command {
        ServiceCommand::Print(options) => {
            let unit = build_unit_options(config_path, &options)?;
            let path = service::unit_path(unit.scope, &unit.name)?;
            println!("# {}", path.display());
            print!("{}", unit.render());
            Ok(())
        }
        ServiceCommand::Install {
            options,
            no_start,
            force,
        } => {
            let unit = build_unit_options(config_path, &options)?;
            if unit.config.is_none() {
                eprintln!(
                    "warning: no config file found; the service will use built-in defaults and environment variables"
                );
            }
            service::install(&unit, force, no_start, options.dry_run)
        }
        ServiceCommand::Uninstall(options) => {
            service::uninstall(&options.name, service_scope(&options), options.dry_run)
        }
        ServiceCommand::Status(options) => service::status(&options.name, service_scope(&options)),
    }
}

fn service_scope(options: &ServiceOptions) -> service::Scope {
    if options.user {
        service::Scope::User
    } else {
        service::Scope::System
    }
}

fn build_unit_options(
    config_path: Option<&std::path::Path>,
    options: &ServiceOptions,
) -> Result<service::UnitOptions> {
    let exec = match &options.bin {
        Some(path) => service::absolute(path)?,
        None => {
            let current = std::env::current_exe()?;
            current.canonicalize().unwrap_or(current)
        }
    };
    let config = service::resolve_config(config_path)?;
    let environment = service::parse_environment(&options.env)?;
    service::UnitOptions::new(
        &options.name,
        exec,
        config,
        environment,
        service_scope(options),
    )
}

fn daemonize(args: &ServeArgs) -> Result<()> {
    let state_dir = default_state_dir();
    std::fs::create_dir_all(&state_dir)?;
    let log_path = args
        .log_file
        .clone()
        .unwrap_or_else(|| state_dir.join("smtpmaster.log"));
    let pid_path = args
        .pid_file
        .clone()
        .unwrap_or_else(|| state_dir.join("smtpmaster.pid"));
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(parent) = pid_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if let Some(existing) = read_pid(&pid_path) {
        if process_alive(existing) {
            return Err(AppError::Conflict(format!(
                "smtpmaster is already running with pid {existing} (pid file {})",
                pid_path.display()
            )));
        }
    }

    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)?;
    let executable = std::env::current_exe()?;
    let mut command = std::process::Command::new(executable);
    command.args(std::env::args_os().skip(1));
    command.env("SMTPMASTER_DAEMONIZED", "1");
    command.stdin(std::process::Stdio::null());
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::from(log));
    // Detach from the controlling terminal and become a session leader.
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let mut child = command
        .spawn()
        .map_err(|error| AppError::Internal(format!("cannot start daemon: {error}")))?;
    let pid = child.id();
    std::fs::write(&pid_path, format!("{pid}\n"))?;
    std::thread::sleep(Duration::from_millis(400));
    if let Some(status) = child.try_wait()? {
        let _ = std::fs::remove_file(&pid_path);
        return Err(AppError::Internal(format!(
            "daemon exited during startup ({status}); see {}",
            log_path.display()
        )));
    }
    println!("smtpmaster running in background (pid {pid})");
    println!("log: {}", log_path.display());
    println!("pid file: {}", pid_path.display());
    Ok(())
}

fn default_state_dir() -> PathBuf {
    if let Some(directory) = std::env::var_os("XDG_STATE_HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(directory).join("smtpmaster");
    }
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return PathBuf::from(home).join(".local/state/smtpmaster");
    }
    PathBuf::from("/tmp/smtpmaster")
}

fn read_pid(path: &Path) -> Option<u32> {
    std::fs::read_to_string(path).ok()?.trim().parse().ok()
}

fn process_alive(pid: u32) -> bool {
    Path::new("/proc").join(pid.to_string()).exists()
}

fn init_tracing(verbose: u8) {
    let default = match verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = EnvFilter::try_from_env("SMTPMASTER_LOG")
        .unwrap_or_else(|_| EnvFilter::new(format!("smtpmaster={default},warn")));
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .try_init();
}

fn resolve_password(
    plain: &Option<String>,
    stdin: bool,
    env_name: &Option<String>,
) -> Result<Option<String>> {
    let sources =
        usize::from(plain.is_some()) + usize::from(stdin) + usize::from(env_name.is_some());
    if sources > 1 {
        return Err(AppError::Invalid(
            "choose only one of --password, --password-stdin, --password-env".into(),
        ));
    }
    if let Some(password) = plain {
        return Ok(Some(password.clone()));
    }
    if stdin {
        let mut buffer = String::new();
        std::io::stdin().read_to_string(&mut buffer)?;
        return Ok(Some(buffer.trim_end_matches(['\r', '\n']).to_string()));
    }
    if let Some(name) = env_name {
        let value = std::env::var(name)
            .map_err(|_| AppError::Invalid(format!("environment variable `{name}` is not set")))?;
        return Ok(Some(value));
    }
    Ok(None)
}

fn expiry_from_flags(
    expires_in: Option<&str>,
    expires_at: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    match (expires_in, expires_at) {
        (Some(_), Some(_)) => Err(AppError::Invalid(
            "use either --expires-in or --expires-at".into(),
        )),
        (Some(spec), None) => Ok(Some(DurationSpec::parse(spec)?.apply(now)?)),
        (None, Some(value)) => Ok(Some(parse_timestamp(value)?)),
        (None, None) => Ok(None),
    }
}

fn parse_timestamp(value: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(value)
        .map(|dt| dt.with_timezone(&Utc))
        .map_err(|_| {
            AppError::Invalid(format!(
                "`{value}` is not an RFC3339 timestamp (e.g. 2026-12-31T23:59:59Z)"
            ))
        })
}

fn normalize_reset_at(value: &str) -> Result<String> {
    let (hour, minute) = parse_reset_at(value)?;
    Ok(format!("{hour:02}:{minute:02}"))
}

fn bdate(ts: DateTime<Utc>) -> Bson {
    Bson::DateTime(bson::DateTime::from(ts))
}

fn bson_of<T: serde::Serialize>(value: &T) -> Result<Bson> {
    bson::serialize_to_bson(value)
        .map_err(|error| AppError::Internal(format!("cannot encode value: {error}")))
}

fn test_message(account: &Account, to: &str) -> Vec<u8> {
    let from = from_label(account);
    let message_id = ObjectId::new().to_hex();
    format!(
        "From: {from}\r\nTo: {to}\r\nSubject: smtpmaster delivery test\r\nDate: {date}\r\nMessage-ID: <{message_id}@smtpmaster>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nThis is a delivery test sent by smtpmaster through account {name}.\r\n",
        date = Utc::now().to_rfc2822(),
        name = account.name,
    )
    .into_bytes()
}

fn from_label(account: &Account) -> String {
    match account.from_name.as_deref() {
        Some(name) if !name.trim().is_empty() => {
            format!("{name} <{}>", account.from_address)
        }
        _ => account.from_address.clone(),
    }
}

fn schedule_label(schedule: &Option<Schedule>) -> String {
    let Some(schedule) = schedule else {
        return "-".to_string();
    };
    if schedule.windows.is_empty() {
        return "-".to_string();
    }
    schedule
        .windows
        .iter()
        .map(|window| {
            let days = if window.days.is_empty() {
                "*".to_string()
            } else {
                window
                    .days
                    .iter()
                    .map(|day| day_name(*day))
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let priority = window
                .priority
                .map(|value| format!("@{value}"))
                .unwrap_or_default();
            format!(
                "{days} {}-{}{priority}",
                hhmm(window.start_min),
                hhmm(window.end_min)
            )
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn day_name(day: u8) -> &'static str {
    match day {
        0 => "mon",
        1 => "tue",
        2 => "wed",
        3 => "thu",
        4 => "fri",
        5 => "sat",
        _ => "sun",
    }
}

fn hhmm(minutes: u32) -> String {
    format!("{:02}:{:02}", minutes / 60, minutes % 60)
}

fn kv(key: &str, value: String) -> (String, String) {
    (key.to_string(), value)
}

fn print_json<T: serde::Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string_pretty(value)?);
    Ok(())
}
