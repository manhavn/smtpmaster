//! Optional token-protected web dashboard.
//!
//! The dashboard is toggled from the CLI by writing a settings document to
//! MongoDB; the running relay polls that document and starts/stops the HTTP
//! listener in-process, so SMTP work is never interrupted.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    Json, Router,
    extract::{Path, Query, Request, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, patch, post},
};
use bson::{Bson, Document, oid::ObjectId};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::duration::DurationSpec;
use crate::error::{AppError, Result};
use crate::model::{Account, AdminSettings, Provider, TlsMode, parse_reset_at, validate_timezone};
use crate::schedule::{Schedule, parse_window};
use crate::smtp;
use crate::store::Store;

const INDEX_HTML: &str = include_str!("../assets/admin.html");

#[derive(Clone)]
struct AdminState {
    store: Arc<Store>,
    token: Arc<String>,
}

type Signature = (bool, String, String);

struct Running {
    signature: Signature,
    handle: JoinHandle<()>,
}

/// Watch the settings document and keep the HTTP listener in sync.
pub async fn watch(store: Arc<Store>, interval: Duration) {
    let interval = interval.max(Duration::from_secs(1));
    let mut running: Option<Running> = None;
    let mut reported_failure: Option<Signature> = None;

    loop {
        match store.admin_settings().await {
            Ok(settings) => {
                let signature = (
                    settings.enabled,
                    settings.listen.clone(),
                    settings.token.clone(),
                );
                let needs_change = running
                    .as_ref()
                    .is_none_or(|current| current.signature != signature);
                if needs_change {
                    if let Some(previous) = running.take() {
                        previous.handle.abort();
                        info!("admin dashboard stopped");
                    }
                    if settings.enabled {
                        if settings.token.is_empty() {
                            if reported_failure.as_ref() != Some(&signature) {
                                warn!(
                                    "admin dashboard is enabled but has no token; refusing to start"
                                );
                                reported_failure = Some(signature);
                            }
                        } else {
                            match start(Arc::clone(&store), &settings).await {
                                Ok(handle) => {
                                    info!(listen = %settings.listen, "admin dashboard listening");
                                    running = Some(Running { signature, handle });
                                    reported_failure = None;
                                }
                                Err(error) => {
                                    if reported_failure.as_ref() != Some(&signature) {
                                        warn!(error = %error, "cannot start admin dashboard");
                                        reported_failure = Some(signature);
                                    }
                                }
                            }
                        }
                    } else {
                        reported_failure = None;
                    }
                }
            }
            Err(error) => warn!(error = %error, "cannot read admin dashboard settings"),
        }
        tokio::time::sleep(interval).await;
    }
}

async fn start(store: Arc<Store>, settings: &AdminSettings) -> Result<JoinHandle<()>> {
    let listener = TcpListener::bind(&settings.listen).await?;
    let address = listener.local_addr()?;
    let app = router(store, settings.token.clone());
    let handle = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, app).await {
            warn!(error = %error, "admin server stopped");
        }
    });
    debug!(%address, "admin dashboard bound");
    Ok(handle)
}

pub fn router(store: Arc<Store>, token: String) -> Router {
    let state = AdminState {
        store,
        token: Arc::new(token),
    };
    let api = Router::new()
        .route("/health", get(health))
        .route("/stats", get(stats))
        .route("/accounts", get(list_accounts).post(create_account))
        .route(
            "/accounts/{key}",
            patch(patch_account).delete(delete_account),
        )
        .route("/accounts/{key}/enable", post(enable_account))
        .route("/accounts/{key}/disable", post(disable_account))
        .route("/accounts/{key}/reset", post(reset_account))
        .route("/accounts/{key}/test", post(test_account))
        .route("/routes", get(list_routes).post(create_route))
        .route("/routes/{key}", patch(patch_route).delete(delete_route))
        .route("/routes/{key}/enable", post(enable_route))
        .route("/routes/{key}/disable", post(disable_route))
        .route("/maintenance", post(run_maintenance))
        .route("/databases", get(list_databases).post(create_database))
        .route("/send_log", get(send_log))
        .layer(middleware::from_fn_with_state(state.clone(), authorize))
        .with_state(state);
    Router::new().route("/", get(index)).nest("/api", api)
}

async fn authorize(
    State(state): State<AdminState>,
    headers: HeaderMap,
    request: Request,
    next: Next,
) -> Response {
    let provided = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim);
    match provided {
        Some(candidate) if token_matches(&state.token, candidate) => next.run(request).await,
        _ => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid or missing token" })),
        )
            .into_response(),
    }
}

/// Constant-time comparison so the token cannot be guessed byte by byte.
fn token_matches(expected: &str, provided: &str) -> bool {
    let expected = expected.as_bytes();
    let provided = provided.as_bytes();
    if expected.len() != provided.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in expected.iter().zip(provided) {
        difference |= left ^ right;
    }
    difference == 0
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn health() -> Json<Value> {
    Json(json!({ "ok": true, "service": "smtpmaster" }))
}

async fn stats(State(state): State<AdminState>) -> ApiResult<Json<Value>> {
    let stats = state.store.stats(Utc::now()).await?;
    Ok(Json(serde_json::to_value(stats)?))
}

async fn list_accounts(State(state): State<AdminState>) -> ApiResult<Json<Value>> {
    let accounts = state.store.list_accounts(false).await?;
    let redacted: Vec<Account> = accounts.iter().map(Account::redacted).collect();
    Ok(Json(json!({ "accounts": redacted })))
}

async fn enable_account(
    State(state): State<AdminState>,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    let account = state.store.find_account(&key).await?;
    let updated = state.store.set_active(account.id, true, None).await?;
    Ok(Json(json!({ "account": updated.redacted() })))
}

async fn disable_account(
    State(state): State<AdminState>,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    let account = state.store.find_account(&key).await?;
    let updated = state
        .store
        .set_active(account.id, false, Some("disabled from admin dashboard"))
        .await?;
    Ok(Json(json!({ "account": updated.redacted() })))
}

async fn reset_account(
    State(state): State<AdminState>,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    let account = state.store.find_account(&key).await?;
    let updated = state.store.reset_usage(&account, Utc::now()).await?;
    Ok(Json(json!({ "account": updated.redacted() })))
}

#[derive(Debug, Deserialize)]
struct AccountCreate {
    name: String,
    username: String,
    password: String,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    tls: Option<String>,
    #[serde(default)]
    from_address: Option<String>,
    #[serde(default)]
    from_name: Option<String>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    count_max: Option<i64>,
    #[serde(default)]
    total_max: Option<i64>,
    #[serde(default)]
    rate_limit: Option<i64>,
    #[serde(default)]
    reset_at: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    expires_in: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    windows: Vec<String>,
    #[serde(default)]
    active: Option<bool>,
}

async fn create_account(
    State(state): State<AdminState>,
    Json(body): Json<AccountCreate>,
) -> ApiResult<Json<Value>> {
    let now = Utc::now();
    let provider = match body.provider.as_deref() {
        Some(value) if !value.is_empty() => value.parse::<Provider>()?,
        _ => Provider::Custom,
    };
    if body.name.trim().is_empty() {
        return Err(ApiError(AppError::Invalid(
            "account name must not be empty".into(),
        )));
    }
    if body.username.trim().is_empty() {
        return Err(ApiError(AppError::Invalid(
            "username must not be empty".into(),
        )));
    }
    if body.password.is_empty() {
        return Err(ApiError(AppError::Invalid(
            "password must not be empty".into(),
        )));
    }
    let from_address = match body
        .from_address
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        Some(value) => value.trim().to_string(),
        None if body.username.contains('@') => body.username.clone(),
        None => {
            return Err(ApiError(AppError::Invalid(
                "from address is required when the username is not an email".into(),
            )));
        }
    };
    if !from_address.contains('@') {
        return Err(ApiError(AppError::Invalid(format!(
            "`{from_address}` is not an email address"
        ))));
    }
    let mut account = Account::new(
        body.name.trim(),
        provider,
        body.username.trim(),
        body.password,
        from_address,
        now,
    );
    match body
        .host
        .as_deref()
        .filter(|value| !value.trim().is_empty())
    {
        Some(host) => account.host = host.trim().to_string(),
        None if provider == Provider::Custom => {
            return Err(ApiError(AppError::Invalid(
                "host is required for smtp accounts".into(),
            )));
        }
        None => {}
    }
    let tls = match body.tls.as_deref().filter(|value| !value.is_empty()) {
        Some(value) => value.parse::<TlsMode>()?,
        None if body.port == Some(465) => TlsMode::Implicit,
        None => TlsMode::Starttls,
    };
    account.tls = tls;
    account.port = body
        .port
        .unwrap_or(if tls == TlsMode::Implicit { 465 } else { 587 });
    account.from_name = body.from_name.filter(|value| !value.trim().is_empty());
    if let Some(priority) = body.priority {
        account.priority = priority;
    }
    account.count_max = non_negative(body.count_max, "count_max")?.unwrap_or(0);
    account.total_max = non_negative(body.total_max, "total_max")?.unwrap_or(0);
    account.rate_limit_per_min = non_negative(body.rate_limit, "rate_limit")?.unwrap_or(0);
    account.rate_tokens = account.rate_limit_per_min as f64;
    if let Some(reset_at) = body.reset_at.as_deref().filter(|value| !value.is_empty()) {
        let (hour, minute) = parse_reset_at(reset_at)?;
        account.reset_at = format!("{hour:02}:{minute:02}");
    }
    if let Some(timezone) = body.timezone.as_deref().filter(|value| !value.is_empty()) {
        validate_timezone(timezone)?;
        account.timezone = timezone.to_string();
    }
    account.expires_at =
        expiry_from_body(body.expires_in.as_deref(), body.expires_at.as_deref(), now)?;
    if !body.windows.is_empty() {
        let windows = body
            .windows
            .iter()
            .filter(|spec| !spec.trim().is_empty())
            .map(|spec| parse_window(spec))
            .collect::<Result<Vec<_>>>()?;
        if !windows.is_empty() {
            account.schedule = Some(Schedule { windows });
        }
    }
    if body.active == Some(false) {
        account.active = false;
        account.disabled_reason = Some("created disabled from dashboard".to_string());
    }
    state.store.insert_account(&account).await?;
    Ok(Json(json!({ "account": account.redacted() })))
}

#[derive(Debug, Deserialize)]
struct AccountUpdate {
    #[serde(default)]
    host: Option<String>,
    #[serde(default)]
    port: Option<u16>,
    #[serde(default)]
    tls: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    from_address: Option<String>,
    #[serde(default)]
    from_name: Option<String>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    count_max: Option<i64>,
    #[serde(default)]
    total_max: Option<i64>,
    #[serde(default)]
    rate_limit: Option<i64>,
    #[serde(default)]
    reset_at: Option<String>,
    #[serde(default)]
    timezone: Option<String>,
    #[serde(default)]
    expires_in: Option<String>,
    #[serde(default)]
    expires_at: Option<String>,
    #[serde(default)]
    clear_expiry: bool,
    #[serde(default)]
    windows: Option<Vec<String>>,
    #[serde(default)]
    active: Option<bool>,
}

async fn patch_account(
    State(state): State<AdminState>,
    Path(key): Path<String>,
    Json(patch): Json<AccountUpdate>,
) -> ApiResult<Json<Value>> {
    let account = state.store.find_account(&key).await?;
    let now = Utc::now();
    if let Some(active) = patch.active {
        state
            .store
            .set_active(
                account.id,
                active,
                if active {
                    None
                } else {
                    Some("disabled from admin dashboard")
                },
            )
            .await?;
    }
    let mut set = Document::new();
    let mut unset = Document::new();
    if let Some(host) = patch.host.filter(|value| !value.trim().is_empty()) {
        set.insert("host", host.trim().to_string());
    }
    if let Some(port) = patch.port {
        set.insert("port", i32::from(port));
    }
    if let Some(tls) = patch.tls.as_deref().filter(|value| !value.is_empty()) {
        set.insert("tls", tls.parse::<TlsMode>()?.as_str());
    }
    if let Some(username) = patch.username.filter(|value| !value.trim().is_empty()) {
        set.insert("username", username.trim().to_string());
    }
    if let Some(password) = patch.password.filter(|value| !value.is_empty()) {
        set.insert("password", password);
    }
    if let Some(from_address) = patch.from_address.filter(|value| !value.trim().is_empty()) {
        if !from_address.contains('@') {
            return Err(ApiError(AppError::Invalid(format!(
                "`{from_address}` is not an email address"
            ))));
        }
        set.insert("from_address", from_address.trim().to_string());
    }
    if let Some(from_name) = patch.from_name {
        if from_name.trim().is_empty() {
            unset.insert("from_name", "");
        } else {
            set.insert("from_name", from_name.trim().to_string());
        }
    }
    if let Some(priority) = patch.priority {
        set.insert("priority", priority);
    }
    if let Some(count_max) = non_negative(patch.count_max, "count_max")? {
        set.insert("count_max", count_max);
    }
    if let Some(total_max) = non_negative(patch.total_max, "total_max")? {
        set.insert("total_max", total_max);
    }
    if let Some(rate_limit) = non_negative(patch.rate_limit, "rate_limit")? {
        set.insert("rate_limit_per_min", rate_limit);
        set.insert("rate_tokens", rate_limit as f64);
        set.insert("rate_updated_at", bdate(now));
    }
    if let Some(reset_at) = patch.reset_at.as_deref().filter(|value| !value.is_empty()) {
        let (hour, minute) = parse_reset_at(reset_at)?;
        set.insert("reset_at", format!("{hour:02}:{minute:02}"));
    }
    if let Some(timezone) = patch.timezone.as_deref().filter(|value| !value.is_empty()) {
        validate_timezone(timezone)?;
        set.insert("timezone", timezone.to_string());
    }
    if patch.clear_expiry {
        unset.insert("expires_at", "");
    } else if let Some(expires_at) = expiry_from_body(
        patch.expires_in.as_deref(),
        patch.expires_at.as_deref(),
        now,
    )? {
        set.insert("expires_at", bdate(expires_at));
    }
    if let Some(windows) = patch.windows {
        let parsed = windows
            .iter()
            .filter(|spec| !spec.trim().is_empty())
            .map(|spec| parse_window(spec))
            .collect::<Result<Vec<_>>>()?;
        if parsed.is_empty() {
            unset.insert("schedule", "");
        } else {
            set.insert(
                "schedule",
                bson::serialize_to_bson(&Schedule { windows: parsed })
                    .map_err(|error| ApiError(AppError::Internal(error.to_string())))?,
            );
        }
    }
    if set.is_empty() && unset.is_empty() {
        return Err(ApiError(AppError::Invalid("nothing to update".into())));
    }
    set.insert("updated_at", bdate(now));
    let mut update = Document::new();
    update.insert("$set", set);
    if !unset.is_empty() {
        update.insert("$unset", unset);
    }
    let updated = state.store.update_account(account.id, update).await?;
    Ok(Json(json!({ "account": updated.redacted() })))
}

async fn delete_account(
    State(state): State<AdminState>,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    let account = state.store.find_account(&key).await?;
    state.store.delete_account(account.id).await?;
    Ok(Json(json!({ "deleted": account.name })))
}

#[derive(Debug, Deserialize)]
struct TestRecipient {
    to: String,
}

async fn test_account(
    State(state): State<AdminState>,
    Path(key): Path<String>,
    Json(body): Json<TestRecipient>,
) -> ApiResult<Json<Value>> {
    if !body.to.contains('@') {
        return Err(ApiError(AppError::Invalid(
            "recipient is not an email address".into(),
        )));
    }
    let account = state.store.find_account(&key).await?;
    let message = test_message(&account, &body.to);
    smtp::send(
        &account,
        &account.from_address,
        std::slice::from_ref(&body.to),
        &message,
        "smtpmaster",
        std::time::Duration::from_secs(30),
    )
    .await?;
    Ok(Json(
        json!({ "ok": true, "account": account.name, "to": body.to }),
    ))
}

async fn run_maintenance(State(state): State<AdminState>) -> ApiResult<Json<Value>> {
    let report = state.store.maintenance(Utc::now()).await?;
    Ok(Json(serde_json::to_value(report)?))
}

#[derive(Debug, Deserialize)]
struct CreateDatabase {
    name: String,
}

async fn list_databases(State(state): State<AdminState>) -> ApiResult<Json<Value>> {
    let databases = state.store.list_databases().await?;
    Ok(Json(json!({
        "current": state.store.database_name(),
        "databases": databases,
    })))
}

async fn create_database(
    State(state): State<AdminState>,
    Json(body): Json<CreateDatabase>,
) -> ApiResult<Json<Value>> {
    state.store.init_database(&body.name).await?;
    Ok(Json(json!({
        "initialized": body.name.trim(),
        "current": state.store.database_name(),
        "note": "the running server keeps using its configured database; use --mongo-db to switch",
    })))
}

#[derive(Debug, Deserialize)]
struct RouteCreate {
    name: String,
    username: String,
    password: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    accounts: Vec<String>,
    #[serde(default)]
    active: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RouteUpdate {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    username: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    accounts: Option<Vec<String>>,
    #[serde(default)]
    active: Option<bool>,
}

async fn resolve_accounts(store: &Store, selectors: &[String]) -> Result<Vec<ObjectId>> {
    let mut ids = Vec::new();
    for selector in selectors {
        let selector = selector.trim();
        if selector.is_empty() {
            continue;
        }
        let account = store.find_account(selector).await?;
        if !ids.contains(&account.id) {
            ids.push(account.id);
        }
    }
    Ok(ids)
}

async fn create_route(
    State(state): State<AdminState>,
    Json(body): Json<RouteCreate>,
) -> ApiResult<Json<Value>> {
    if body.name.trim().is_empty() || body.username.trim().is_empty() {
        return Err(ApiError(AppError::Invalid(
            "route name and inbound username must not be empty".into(),
        )));
    }
    if body.password.is_empty() {
        return Err(ApiError(AppError::Invalid(
            "inbound password must not be empty".into(),
        )));
    }
    let account_ids = resolve_accounts(&state.store, &body.accounts)
        .await
        .map_err(ApiError)?;
    if account_ids.is_empty() {
        return Err(ApiError(AppError::Invalid(
            "a route needs at least one outbound account".into(),
        )));
    }
    let mut route = crate::route::Route::new(
        body.name.trim(),
        body.username.trim(),
        body.password,
        account_ids,
        Utc::now(),
    );
    route.description = body.description.filter(|value| !value.trim().is_empty());
    if body.active == Some(false) {
        route.active = false;
    }
    state.store.insert_route(&route).await?;
    Ok(Json(json!({ "route": route.redacted() })))
}

async fn patch_route(
    State(state): State<AdminState>,
    Path(key): Path<String>,
    Json(patch): Json<RouteUpdate>,
) -> ApiResult<Json<Value>> {
    let route = state.store.find_route(&key).await?;
    let mut set = Document::new();
    let mut unset = Document::new();
    if let Some(name) = patch.name.filter(|value| !value.trim().is_empty()) {
        set.insert("name", name.trim().to_string());
    }
    if let Some(username) = patch.username.filter(|value| !value.trim().is_empty()) {
        set.insert("username", username.trim().to_string());
    }
    if let Some(password) = patch.password.filter(|value| !value.is_empty()) {
        set.insert("password", password);
    }
    if let Some(description) = patch.description {
        if description.trim().is_empty() {
            unset.insert("description", "");
        } else {
            set.insert("description", description.trim().to_string());
        }
    }
    if let Some(accounts) = patch.accounts {
        let ids = resolve_accounts(&state.store, &accounts)
            .await
            .map_err(ApiError)?;
        if ids.is_empty() {
            return Err(ApiError(AppError::Invalid(
                "a route needs at least one outbound account".into(),
            )));
        }
        set.insert("account_ids", ids);
    }
    if let Some(active) = patch.active {
        set.insert("active", active);
    }
    if set.is_empty() && unset.is_empty() {
        return Err(ApiError(AppError::Invalid("nothing to update".into())));
    }
    set.insert("updated_at", bdate(Utc::now()));
    let mut update = Document::new();
    update.insert("$set", set);
    if !unset.is_empty() {
        update.insert("$unset", unset);
    }
    let updated = state.store.update_route(route.id, update).await?;
    Ok(Json(json!({ "route": updated.redacted() })))
}

async fn delete_route(
    State(state): State<AdminState>,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    let route = state.store.find_route(&key).await?;
    state.store.delete_route(route.id).await?;
    Ok(Json(json!({ "deleted": route.name })))
}

async fn list_routes(State(state): State<AdminState>) -> ApiResult<Json<Value>> {
    let routes = state.store.list_routes().await?;
    let names: HashMap<ObjectId, String> = state
        .store
        .list_accounts(false)
        .await?
        .into_iter()
        .map(|account| (account.id, account.name))
        .collect();
    let items: Vec<Value> = routes
        .iter()
        .map(|route| {
            let accounts: Vec<Value> = route
                .account_ids
                .iter()
                .map(|id| {
                    json!({
                        "id": id.to_hex(),
                        "name": names.get(id),
                    })
                })
                .collect();
            json!({
                "id": route.id.to_hex(),
                "name": route.name,
                "username": route.username,
                "active": route.active,
                "description": route.description,
                "accounts": accounts,
                "updated_at": route.updated_at.to_rfc3339_opts(SecondsFormat::Secs, true),
            })
        })
        .collect();
    Ok(Json(json!({ "routes": items })))
}

async fn enable_route(
    State(state): State<AdminState>,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    let route = state.store.find_route(&key).await?;
    let updated = state.store.set_route_active(route.id, true).await?;
    Ok(Json(json!({
        "route": updated.redacted(),
        "account_ids": updated.account_ids.iter().map(|id| id.to_hex()).collect::<Vec<_>>(),
    })))
}

async fn disable_route(
    State(state): State<AdminState>,
    Path(key): Path<String>,
) -> ApiResult<Json<Value>> {
    let route = state.store.find_route(&key).await?;
    let updated = state.store.set_route_active(route.id, false).await?;
    Ok(Json(json!({
        "route": updated.redacted(),
        "account_ids": updated.account_ids.iter().map(|id| id.to_hex()).collect::<Vec<_>>(),
    })))
}

#[derive(Debug, Default, Deserialize)]
struct LogQuery {
    limit: Option<i64>,
}

async fn send_log(
    State(state): State<AdminState>,
    Query(query): Query<LogQuery>,
) -> ApiResult<Json<Value>> {
    let logs = state
        .store
        .recent_send_log(query.limit.unwrap_or(100))
        .await?;
    let items: Vec<Value> = logs
        .iter()
        .map(|log| {
            json!({
                "id": log.id.to_hex(),
                "account": log.account_name,
                "from": log.from,
                "recipients": log.recipients,
                "status": log.status,
                "error": log.error,
                "attempts": log.attempts,
                "duration_ms": log.duration_ms,
                "ts": log.ts.to_rfc3339_opts(SecondsFormat::Millis, true),
            })
        })
        .collect();
    Ok(Json(json!({ "logs": items })))
}

fn non_negative(value: Option<i64>, field: &str) -> Result<Option<i64>> {
    match value {
        Some(value) if value < 0 => Err(AppError::Invalid(format!("{field} cannot be negative"))),
        other => Ok(other),
    }
}

fn expiry_from_body(
    expires_in: Option<&str>,
    expires_at: Option<&str>,
    now: DateTime<Utc>,
) -> Result<Option<DateTime<Utc>>> {
    let expires_in = expires_in.filter(|value| !value.trim().is_empty());
    let expires_at = expires_at.filter(|value| !value.trim().is_empty());
    match (expires_in, expires_at) {
        (Some(_), Some(_)) => Err(AppError::Invalid(
            "use either expires_in or expires_at".into(),
        )),
        (Some(spec), None) => Ok(Some(DurationSpec::parse(spec)?.apply(now)?)),
        (None, Some(value)) => DateTime::parse_from_rfc3339(value)
            .map(|value| Some(value.with_timezone(&Utc)))
            .map_err(|_| {
                AppError::Invalid(format!(
                    "`{value}` is not an RFC3339 timestamp (e.g. 2026-12-31T23:59:59Z)"
                ))
            }),
        (None, None) => Ok(None),
    }
}

fn test_message(account: &Account, to: &str) -> Vec<u8> {
    let from = match account.from_name.as_deref() {
        Some(name) if !name.trim().is_empty() => {
            format!("{name} <{}>", account.from_address)
        }
        _ => account.from_address.clone(),
    };
    let message_id = ObjectId::new().to_hex();
    format!(
        "From: {from}\r\nTo: {to}\r\nSubject: smtpmaster dashboard test\r\nDate: {date}\r\nMessage-ID: <{message_id}@smtpmaster>\r\nMIME-Version: 1.0\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nThis is a delivery test sent from the smtpmaster dashboard through account {name}.\r\n",
        date = Utc::now().to_rfc2822(),
        name = account.name,
    )
    .into_bytes()
}

type ApiResult<T> = std::result::Result<T, ApiError>;

struct ApiError(AppError);

impl From<AppError> for ApiError {
    fn from(error: AppError) -> Self {
        Self(error)
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(error: serde_json::Error) -> Self {
        Self(AppError::Json(error))
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            AppError::NotFound(_) => StatusCode::NOT_FOUND,
            AppError::Invalid(_) | AppError::Config(_) => StatusCode::BAD_REQUEST,
            AppError::Conflict(_) => StatusCode::CONFLICT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (status, Json(json!({ "error": self.0.to_string() }))).into_response()
    }
}

fn bdate(value: DateTime<Utc>) -> Bson {
    Bson::DateTime(bson::DateTime::from(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_comparison_is_exact() {
        assert!(token_matches("abcdef", "abcdef"));
        assert!(!token_matches("abcdef", "abcdeg"));
        assert!(!token_matches("abcdef", "abcde"));
        assert!(!token_matches("abcdef", ""));
    }

    #[test]
    fn masks_tokens() {
        let mut settings = AdminSettings::defaults();
        assert_eq!(settings.token_masked(), "-");
        settings.token = "supersecret".to_string();
        assert_eq!(settings.token_masked(), "supe…");
    }
}
