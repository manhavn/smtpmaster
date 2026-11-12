//! MongoDB-backed quota, rate-limit, expiry and CRUD tests.
//!
//! Run with `SMTPMASTER_TEST_MONGO_URI=mongodb://127.0.0.1:27017 cargo test`.

use bson::{Bson, doc, oid::ObjectId};
use chrono::{TimeDelta, Utc};
use smtpmaster::error::AppError;
use smtpmaster::model::{Account, Provider, TlsMode};
use smtpmaster::route::Route;
use smtpmaster::store::Store;

fn test_uri() -> Option<String> {
    std::env::var("SMTPMASTER_TEST_MONGO_URI").ok()
}

async fn test_store() -> Option<Store> {
    let uri = test_uri()?;
    let database = format!("smtpmaster_test_{}", ObjectId::new().to_hex());
    let store = Store::connect(&uri, &database, 1)
        .await
        .expect("connect to test MongoDB");
    store.ensure_indexes().await.expect("create indexes");
    Some(store)
}

async fn cleanup(store: &Store) {
    let _ = store.database().drop().await;
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
    account.count_max = 2;
    account
}

fn date(value: chrono::DateTime<Utc>) -> Bson {
    Bson::DateTime(bson::DateTime::from(value))
}

#[tokio::test]
async fn reservation_enforces_quota_and_rolls_the_window() {
    let Some(store) = test_store().await else {
        eprintln!("skipping: SMTPMASTER_TEST_MONGO_URI not set");
        return;
    };
    let now = Utc::now();
    let account = account("quota");
    store.insert_account(&account).await.unwrap();

    let first = store.reserve(&account, now).await.unwrap().expect("first");
    assert_eq!(first.count_used, 1);
    let second = store.reserve(&account, now).await.unwrap().expect("second");
    assert_eq!(second.count_used, 2);
    assert!(
        store.reserve(&account, now).await.unwrap().is_none(),
        "quota must be exhausted"
    );

    // A completed send does not consume extra quota.
    store.mark_success(account.id).await.unwrap();
    let stored = store.find_account("quota").await.unwrap();
    assert_eq!(stored.count_used, 2);
    assert_eq!(stored.total_sent, 1);

    // Roll the window back two days: the next reservation resets the counter.
    store
        .update_account(
            account.id,
            doc! { "$set": { "window_start": date(now - TimeDelta::days(2)) } },
        )
        .await
        .unwrap();
    let refreshed = store.reserve(&account, now).await.unwrap().expect("reset");
    assert_eq!(refreshed.count_used, 1);

    // A failed send refunds its reservation.
    store.mark_failure(account.id, "boom", 5).await.unwrap();
    let after_failure = store.find_account("quota").await.unwrap();
    assert_eq!(after_failure.count_used, 0);
    assert_eq!(after_failure.consecutive_failures, 1);

    cleanup(&store).await;
}

#[tokio::test]
async fn lifetime_package_cap_blocks_sends() {
    let Some(store) = test_store().await else {
        eprintln!("skipping: SMTPMASTER_TEST_MONGO_URI not set");
        return;
    };
    let now = Utc::now();
    let mut account = account("package");
    account.count_max = 0;
    account.total_max = 2;
    account.total_sent = 2;
    store.insert_account(&account).await.unwrap();

    assert!(
        store.reserve(&account, now).await.unwrap().is_none(),
        "lifetime cap reached"
    );

    store
        .update_account(account.id, doc! { "$set": { "total_sent": 1i64 } })
        .await
        .unwrap();
    assert!(store.reserve(&account, now).await.unwrap().is_some());

    cleanup(&store).await;
}

#[tokio::test]
async fn rate_limit_uses_a_refilling_token_bucket() {
    let Some(store) = test_store().await else {
        eprintln!("skipping: SMTPMASTER_TEST_MONGO_URI not set");
        return;
    };
    let now = Utc::now();
    let mut account = account("rate");
    account.count_max = 0;
    account.rate_limit_per_min = 2;
    account.rate_tokens = 2.0;
    account.rate_updated_at = now;
    store.insert_account(&account).await.unwrap();

    assert!(store.reserve(&account, now).await.unwrap().is_some());
    assert!(store.reserve(&account, now).await.unwrap().is_some());
    assert!(
        store.reserve(&account, now).await.unwrap().is_none(),
        "burst of 2 must be exhausted"
    );

    // 61 seconds later the bucket refilled beyond one token.
    store
        .update_account(
            account.id,
            doc! { "$set": {
                "rate_tokens": 0.0f64,
                "rate_updated_at": date(now - TimeDelta::seconds(61)),
            } },
        )
        .await
        .unwrap();
    let later = now + TimeDelta::seconds(61);
    assert!(store.reserve(&account, later).await.unwrap().is_some());

    cleanup(&store).await;
}

#[tokio::test]
async fn expired_accounts_are_excluded_then_disabled() {
    let Some(store) = test_store().await else {
        eprintln!("skipping: SMTPMASTER_TEST_MONGO_URI not set");
        return;
    };
    let now = Utc::now();
    let mut account = account("expiring");
    account.expires_at = Some(now - TimeDelta::days(1));
    store.insert_account(&account).await.unwrap();

    assert!(store.reserve(&account, now).await.unwrap().is_none());

    let report = store.maintenance(now).await.unwrap();
    assert_eq!(report.accounts_disabled, 1);
    let stored = store.find_account("expiring").await.unwrap();
    assert!(!stored.active);
    assert!(
        stored
            .disabled_reason
            .as_deref()
            .unwrap_or_default()
            .contains("expired")
    );

    cleanup(&store).await;
}

#[tokio::test]
async fn maintenance_resets_stale_quota_windows_only() {
    let Some(store) = test_store().await else {
        eprintln!("skipping: SMTPMASTER_TEST_MONGO_URI not set");
        return;
    };
    let now = Utc::now();
    let mut stale = account("stale");
    stale.count_used = 5;
    stale.window_start = now - TimeDelta::days(3);
    stale.reset_at = "00:00".to_string();
    stale.timezone = "UTC".to_string();
    store.insert_account(&stale).await.unwrap();

    let mut fresh = account("fresh");
    fresh.count_used = 0;
    fresh.window_start = now;
    store.insert_account(&fresh).await.unwrap();

    let report = store.maintenance(now).await.unwrap();
    assert_eq!(report.windows_reset, 1);

    let stored = store.find_account("stale").await.unwrap();
    assert_eq!(stored.count_used, 0);
    assert!(stored.window_start > now - TimeDelta::days(1));
    let untouched = store.find_account("fresh").await.unwrap();
    assert_eq!(untouched.count_used, 0);

    cleanup(&store).await;
}

#[tokio::test]
async fn crud_and_unique_names() {
    let Some(store) = test_store().await else {
        eprintln!("skipping: SMTPMASTER_TEST_MONGO_URI not set");
        return;
    };
    let original = account("crud");
    store.insert_account(&original).await.unwrap();

    let found = store.find_account("crud").await.unwrap();
    assert_eq!(found.id, original.id);
    let by_id = store.find_account(&original.id.to_hex()).await.unwrap();
    assert_eq!(by_id.name, "crud");

    let duplicate = account("crud");
    match store.insert_account(&duplicate).await {
        Err(AppError::Conflict(message)) => assert!(message.contains("crud")),
        other => panic!("expected conflict, got {other:?}"),
    }

    let updated = store
        .update_account(
            original.id,
            doc! { "$set": { "priority": 7i32, "count_max": 42i64 } },
        )
        .await
        .unwrap();
    assert_eq!(updated.priority, 7);
    assert_eq!(updated.count_max, 42);

    store
        .set_active(original.id, false, Some("test"))
        .await
        .unwrap();
    let disabled = store.find_account("crud").await.unwrap();
    assert!(!disabled.active);
    store.set_active(original.id, true, None).await.unwrap();
    let enabled = store.find_account("crud").await.unwrap();
    assert!(enabled.active);
    assert!(enabled.disabled_reason.is_none());

    store.delete_account(original.id).await.unwrap();
    assert!(matches!(
        store.find_account("crud").await,
        Err(AppError::NotFound(_))
    ));

    cleanup(&store).await;
}

#[tokio::test]
async fn route_persistence_and_authentication() {
    let Some(store) = test_store().await else {
        eprintln!("skipping: SMTPMASTER_TEST_MONGO_URI not set");
        return;
    };
    let first = account("route-a");
    let second = account("route-b");
    store.insert_account(&first).await.unwrap();
    store.insert_account(&second).await.unwrap();

    let now = Utc::now();
    let route = Route::new("app", "inbound", "secret", vec![first.id, second.id], now);
    store.insert_route(&route).await.unwrap();
    assert!(store.routes_active().await.unwrap());

    let found = store.find_route("app").await.unwrap();
    assert_eq!(found.account_ids, vec![first.id, second.id]);
    assert!(
        store
            .authenticate_route("inbound", "secret")
            .await
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .authenticate_route("inbound", "wrong")
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        store
            .authenticate_route("nobody", "secret")
            .await
            .unwrap()
            .is_none()
    );

    let duplicate = Route::new("other", "inbound", "x", Vec::new(), now);
    match store.insert_route(&duplicate).await {
        Err(AppError::Conflict(message)) => assert!(message.contains("route")),
        other => panic!("expected conflict, got {other:?}"),
    }

    store.set_route_active(route.id, false).await.unwrap();
    assert!(
        store
            .authenticate_route("inbound", "secret")
            .await
            .unwrap()
            .is_none()
    );
    store.set_route_active(route.id, true).await.unwrap();

    let account_ids = bson::serialize_to_bson(&vec![second.id]).unwrap();
    let updated = store
        .update_route(route.id, doc! { "$set": { "account_ids": account_ids } })
        .await
        .unwrap();
    assert_eq!(updated.account_ids, vec![second.id]);

    store.delete_route(route.id).await.unwrap();
    assert!(!store.routes_active().await.unwrap());
    assert!(matches!(
        store.find_route("app").await,
        Err(AppError::NotFound(_))
    ));

    cleanup(&store).await;
}
