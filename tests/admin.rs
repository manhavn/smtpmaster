//! Web dashboard API and runtime-toggle tests.
//!
//! Run with `SMTPMASTER_TEST_MONGO_URI=mongodb://127.0.0.1:27017 cargo test`.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use bson::{doc, oid::ObjectId};
use chrono::Utc;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use smtpmaster::admin;
use smtpmaster::model::{Account, AdminSettings, Provider, TlsMode};
use smtpmaster::route::Route;
use smtpmaster::store::Store;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tower::ServiceExt;

const TOKEN: &str = "test-token-1234567890";

fn test_uri() -> Option<String> {
    std::env::var("SMTPMASTER_TEST_MONGO_URI").ok()
}

async fn test_store(prefix: &str) -> Option<Arc<Store>> {
    let uri = test_uri()?;
    let database = format!("{prefix}_{}", ObjectId::new().to_hex());
    let store = Store::connect(&uri, &database, 1)
        .await
        .expect("connect to test MongoDB");
    store.ensure_indexes().await.expect("create indexes");
    Some(Arc::new(store))
}

fn account(name: &str) -> Account {
    let now = Utc::now();
    let mut account = Account::new(
        name,
        Provider::Custom,
        "user@example.com",
        "secret",
        "user@example.com",
        now,
    );
    account.host = "127.0.0.1".to_string();
    account.port = 2525;
    account.tls = TlsMode::None;
    account.count_max = 10;
    account
}

async fn call(
    app: &Router,
    method: Method,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let body = match body {
        Some(value) => {
            builder = builder.header("content-type", "application/json");
            Body::from(value.to_string())
        }
        None => Body::empty(),
    };
    let response = app
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let value = serde_json::from_slice(&bytes)
        .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).to_string()));
    (status, value)
}

#[tokio::test]
async fn dashboard_requires_a_token() {
    let Some(store) = test_store("smtpmaster_admin_auth").await else {
        eprintln!("skipping: SMTPMASTER_TEST_MONGO_URI not set");
        return;
    };
    let app = admin::router(Arc::clone(&store), TOKEN.to_string());

    let (status, body) = call(&app, Method::GET, "/", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        body.as_str().unwrap_or_default().contains("smtpmaster"),
        "dashboard shell is served"
    );

    let (status, _) = call(&app, Method::GET, "/api/stats", None, None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, _) = call(&app, Method::GET, "/api/stats", Some("wrong-token"), None).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    let (status, body) = call(&app, Method::GET, "/api/stats", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("total_accounts").is_some());

    let _ = store.database().drop().await;
}

#[tokio::test]
async fn dashboard_manages_accounts_and_routes() {
    let Some(store) = test_store("smtpmaster_admin_api").await else {
        eprintln!("skipping: SMTPMASTER_TEST_MONGO_URI not set");
        return;
    };
    store
        .insert_account(&account("admin-account"))
        .await
        .unwrap();
    let app = admin::router(Arc::clone(&store), TOKEN.to_string());

    let (status, body) = call(&app, Method::GET, "/api/accounts", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    let accounts = body.get("accounts").unwrap().as_array().unwrap();
    assert_eq!(accounts.len(), 1);
    assert_eq!(accounts[0]["password"], "***");
    assert_eq!(accounts[0]["name"], "admin-account");

    let (status, _) = call(
        &app,
        Method::PATCH,
        "/api/accounts/admin-account",
        Some(TOKEN),
        Some(json!({"count_max": 7, "rate_limit": 30, "priority": 3})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let stored = store.find_account("admin-account").await.unwrap();
    assert_eq!(stored.count_max, 7);
    assert_eq!(stored.rate_limit_per_min, 30);
    assert_eq!(stored.priority, 3);

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/accounts/admin-account/disable",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!store.find_account("admin-account").await.unwrap().active);
    let (status, _) = call(
        &app,
        Method::POST,
        "/api/accounts/admin-account/enable",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(store.find_account("admin-account").await.unwrap().active);

    store
        .update_account(
            store.find_account("admin-account").await.unwrap().id,
            doc! { "$set": { "count_used": 5i64 } },
        )
        .await
        .unwrap();
    let (status, _) = call(
        &app,
        Method::POST,
        "/api/accounts/admin-account/reset",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        store
            .find_account("admin-account")
            .await
            .unwrap()
            .count_used,
        0
    );

    let stored = store.find_account("admin-account").await.unwrap();
    let route = Route::new("admin-route", "app", "secret", vec![stored.id], Utc::now());
    store.insert_route(&route).await.unwrap();

    let (status, body) = call(&app, Method::GET, "/api/routes", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    let routes = body.get("routes").unwrap().as_array().unwrap();
    assert_eq!(routes[0]["name"], "admin-route");
    assert_eq!(routes[0]["accounts"][0]["name"], "admin-account");

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/routes/admin-route/disable",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(!store.find_route("admin-route").await.unwrap().active);

    let (status, body) = call(
        &app,
        Method::GET,
        "/api/send_log?limit=5",
        Some(TOKEN),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body.get("logs").unwrap().is_array());

    let _ = store.database().drop().await;
}

fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

async fn http_status(port: u16, path: &str, token: &str) -> std::io::Result<u16> {
    let mut stream = tokio::time::timeout(
        Duration::from_secs(3),
        TcpStream::connect(("127.0.0.1", port)),
    )
    .await
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "connect timeout"))??;
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nConnection: close\r\n\r\n"
    );
    stream.write_all(request.as_bytes()).await?;
    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), stream.read_to_end(&mut response))
        .await
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::TimedOut, "read timeout"))??;
    let text = String::from_utf8_lossy(&response);
    let status = text
        .split_whitespace()
        .nth(1)
        .and_then(|value| value.parse::<u16>().ok())
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "no status line"))?;
    Ok(status)
}

#[tokio::test]
async fn watch_toggles_dashboard_without_restarting() {
    let Some(store) = test_store("smtpmaster_admin_watch").await else {
        eprintln!("skipping: SMTPMASTER_TEST_MONGO_URI not set");
        return;
    };
    let port = free_port();
    let settings = AdminSettings {
        id: AdminSettings::ID.to_string(),
        enabled: true,
        listen: format!("127.0.0.1:{port}"),
        token: TOKEN.to_string(),
        updated_at: Utc::now(),
    };
    store.save_admin_settings(&settings).await.unwrap();

    let watcher = tokio::spawn(admin::watch(Arc::clone(&store), Duration::from_secs(1)));

    let mut started = false;
    for _ in 0..40 {
        if matches!(http_status(port, "/api/health", TOKEN).await, Ok(200)) {
            started = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(started, "dashboard did not start after enabling settings");

    let mut disabled = settings.clone();
    disabled.enabled = false;
    disabled.updated_at = Utc::now();
    store.save_admin_settings(&disabled).await.unwrap();

    let mut stopped = false;
    for _ in 0..60 {
        if http_status(port, "/api/health", TOKEN).await.is_err() {
            stopped = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(stopped, "dashboard listener did not stop after disabling");

    watcher.abort();
    let _ = store.database().drop().await;
}

#[tokio::test]
async fn dashboard_lists_and_creates_databases() {
    let Some(store) = test_store("smtpmaster_admin_db").await else {
        eprintln!("skipping: SMTPMASTER_TEST_MONGO_URI not set");
        return;
    };
    let app = admin::router(Arc::clone(&store), TOKEN.to_string());

    let (status, body) = call(&app, Method::GET, "/api/databases", Some(TOKEN), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["current"], store.database_name());
    assert!(body["databases"].is_array());

    let (status, body) = call(
        &app,
        Method::POST,
        "/api/databases",
        Some(TOKEN),
        Some(json!({"name": "created_by_dashboard"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["initialized"], "created_by_dashboard");

    let (_, body) = call(&app, Method::GET, "/api/databases", Some(TOKEN), None).await;
    let databases = body["databases"].as_array().unwrap();
    let created = databases
        .iter()
        .find(|database| database["name"] == "created_by_dashboard")
        .expect("created database is listed");
    assert!(created["collections"].as_u64().unwrap() >= 3);

    let (status, _) = call(
        &app,
        Method::POST,
        "/api/databases",
        Some(TOKEN),
        Some(json!({"name": "bad/name"})),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let _ = store
        .database()
        .client()
        .database("created_by_dashboard")
        .drop()
        .await;
    let _ = store.database().drop().await;
}
