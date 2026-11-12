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

use crate::error::{AppError, Result};
use crate::model::{Account, AdminSettings};
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
        .route("/accounts", get(list_accounts))
        .route("/accounts/{key}", patch(patch_account))
        .route("/accounts/{key}/enable", post(enable_account))
        .route("/accounts/{key}/disable", post(disable_account))
        .route("/accounts/{key}/reset", post(reset_account))
        .route("/routes", get(list_routes))
        .route("/routes/{key}/enable", post(enable_route))
        .route("/routes/{key}/disable", post(disable_route))
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

#[derive(Debug, Deserialize)]
struct AccountPatch {
    #[serde(default)]
    count_max: Option<i64>,
    #[serde(default)]
    total_max: Option<i64>,
    #[serde(default)]
    rate_limit: Option<i64>,
    #[serde(default)]
    priority: Option<i32>,
    #[serde(default)]
    active: Option<bool>,
}

async fn patch_account(
    State(state): State<AdminState>,
    Path(key): Path<String>,
    Json(patch): Json<AccountPatch>,
) -> ApiResult<Json<Value>> {
    let account = state.store.find_account(&key).await?;
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
    if let Some(count_max) = patch.count_max {
        if count_max < 0 {
            return Err(ApiError(AppError::Invalid(
                "count_max cannot be negative".into(),
            )));
        }
        set.insert("count_max", count_max);
    }
    if let Some(total_max) = patch.total_max {
        if total_max < 0 {
            return Err(ApiError(AppError::Invalid(
                "total_max cannot be negative".into(),
            )));
        }
        set.insert("total_max", total_max);
    }
    if let Some(rate_limit) = patch.rate_limit {
        if rate_limit < 0 {
            return Err(ApiError(AppError::Invalid(
                "rate_limit cannot be negative".into(),
            )));
        }
        set.insert("rate_limit_per_min", rate_limit);
        set.insert("rate_tokens", rate_limit as f64);
        set.insert("rate_updated_at", bdate(Utc::now()));
    }
    if let Some(priority) = patch.priority {
        set.insert("priority", priority);
    }
    if !set.is_empty() {
        set.insert("updated_at", bdate(Utc::now()));
        state.store.update_account(account.id, doc_set(set)).await?;
    }
    let updated = state.store.find_account(&key).await?;
    Ok(Json(json!({ "account": updated.redacted() })))
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

fn doc_set(set: Document) -> Document {
    bson::doc! { "$set": set }
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
