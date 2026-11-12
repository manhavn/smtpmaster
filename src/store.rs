//! MongoDB persistence and the atomic quota/rate reservation.

use std::time::Duration;

use bson::{Bson, Document, doc, oid::ObjectId};
use chrono::{DateTime, TimeDelta, Utc};
use mongodb::{
    Client, Collection, Database,
    error::{ErrorKind, WriteFailure},
    options::{ClientOptions, IndexOptions, ReturnDocument},
};
use serde::{Deserialize, Serialize};
use tracing::debug;

use crate::error::{AppError, Result};
use crate::model::Account;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SendLog {
    #[serde(rename = "_id")]
    pub id: ObjectId,
    pub account_id: ObjectId,
    pub account_name: String,
    pub from: String,
    pub recipients: Vec<String>,
    /// `sent` or `failed`.
    pub status: String,
    pub error: Option<String>,
    pub attempts: u32,
    pub duration_ms: u64,
    #[serde(with = "crate::serde_bson::datetime")]
    pub ts: DateTime<Utc>,
}

#[derive(Debug, Default, Clone, Copy, Serialize)]
pub struct MaintenanceReport {
    pub windows_reset: u64,
    pub accounts_disabled: u64,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct Stats {
    pub total_accounts: u64,
    pub active_accounts: u64,
    pub expired_accounts: u64,
    pub disabled_accounts: u64,
    /// Quota used by accounts that have a cap.
    pub used_current_window: i64,
    /// Sum of `count_max` across capped accounts.
    pub capacity_current_window: i64,
    /// Accounts without a quota cap.
    pub unlimited_accounts: u64,
    pub sent_24h: u64,
    pub failed_24h: u64,
}

pub struct Store {
    db: Database,
    retention_days: u32,
}

impl Store {
    pub async fn connect(uri: &str, database: &str, retention_days: u32) -> Result<Self> {
        let mut options = ClientOptions::parse(uri)
            .await
            .map_err(|e| AppError::Config(format!("invalid mongodb uri: {e}")))?;
        options.app_name = Some("smtpmaster".to_string());
        options.server_selection_timeout = Some(Duration::from_secs(5));
        options.connect_timeout = Some(Duration::from_secs(10));
        let client = Client::with_options(options)
            .map_err(|e| AppError::Config(format!("cannot create mongodb client: {e}")))?;
        let db = client.database(database);
        db.run_command(doc! { "ping": 1 })
            .await
            .map_err(|e| AppError::Config(format!("cannot reach mongodb at {uri}: {e}")))?;
        Ok(Self { db, retention_days })
    }

    pub fn database(&self) -> &Database {
        &self.db
    }

    fn accounts(&self) -> Collection<Account> {
        self.db.collection::<Account>("accounts")
    }

    fn logs(&self) -> Collection<SendLog> {
        self.db.collection::<SendLog>("send_log")
    }

    pub async fn ensure_indexes(&self) -> Result<()> {
        self.accounts()
            .create_index(
                mongodb::IndexModel::builder()
                    .keys(doc! { "name": 1 })
                    .options(
                        IndexOptions::builder()
                            .unique(true)
                            .name("uniq_name".to_string())
                            .build(),
                    )
                    .build(),
            )
            .await?;
        self.accounts()
            .create_index(
                mongodb::IndexModel::builder()
                    .keys(doc! { "active": 1, "priority": 1, "last_used_at": 1 })
                    .options(IndexOptions::builder().name("routing".to_string()).build())
                    .build(),
            )
            .await?;
        self.accounts()
            .create_index(
                mongodb::IndexModel::builder()
                    .keys(doc! { "expires_at": 1 })
                    .options(IndexOptions::builder().name("expiry".to_string()).build())
                    .build(),
            )
            .await?;
        self.logs()
            .create_index(
                mongodb::IndexModel::builder()
                    .keys(doc! { "ts": 1 })
                    .options(
                        IndexOptions::builder()
                            .name("log_ttl".to_string())
                            .expire_after(Some(Duration::from_secs(
                                u64::from(self.retention_days.max(1)) * 86_400,
                            )))
                            .build(),
                    )
                    .build(),
            )
            .await?;
        self.logs()
            .create_index(
                mongodb::IndexModel::builder()
                    .keys(doc! { "account_id": 1, "ts": -1 })
                    .options(
                        IndexOptions::builder()
                            .name("log_account".to_string())
                            .build(),
                    )
                    .build(),
            )
            .await?;
        Ok(())
    }

    pub async fn insert_account(&self, account: &Account) -> Result<()> {
        match self.accounts().insert_one(account).await {
            Ok(_) => Ok(()),
            Err(e) => Err(map_write_error(e, "account", &account.name)),
        }
    }

    pub async fn save_account(&self, account: &Account) -> Result<Account> {
        let mut replacement = account.clone();
        replacement.updated_at = Utc::now();
        let result = self
            .accounts()
            .replace_one(doc! { "_id": account.id }, &replacement)
            .await;
        match result {
            Ok(_) => Ok(replacement),
            Err(e) => Err(map_write_error(e, "account", &account.name)),
        }
    }

    pub async fn list_accounts(&self, active_only: bool) -> Result<Vec<Account>> {
        let filter = if active_only {
            doc! { "active": true }
        } else {
            doc! {}
        };
        let mut cursor = self
            .accounts()
            .find(filter)
            .sort(doc! { "priority": 1, "last_used_at": 1, "name": 1 })
            .await?;
        let mut out = Vec::new();
        while cursor.advance().await? {
            out.push(cursor.deserialize_current()?);
        }
        Ok(out)
    }

    pub async fn find_account(&self, key: &str) -> Result<Account> {
        let key = key.trim();
        if key.is_empty() {
            return Err(AppError::Invalid("account name is empty".into()));
        }
        let filter = match ObjectId::parse_str(key) {
            Ok(id) => doc! { "$or": [ { "_id": id }, { "name": key } ] },
            Err(_) => doc! { "name": key },
        };
        self.accounts()
            .find_one(filter)
            .await?
            .ok_or_else(|| AppError::NotFound(format!("account `{key}`")))
    }

    pub async fn update_account(&self, id: ObjectId, update: Document) -> Result<Account> {
        if update.is_empty() {
            return Err(AppError::Invalid("nothing to update".into()));
        }
        let result = self
            .accounts()
            .find_one_and_update(doc! { "_id": id }, update)
            .return_document(ReturnDocument::After)
            .await;
        match result {
            Ok(Some(account)) => Ok(account),
            Ok(None) => Err(AppError::NotFound(format!("account {id}"))),
            Err(e) => Err(map_write_error(e, "account", &id.to_hex())),
        }
    }

    pub async fn delete_account(&self, id: ObjectId) -> Result<()> {
        let result = self.accounts().delete_one(doc! { "_id": id }).await?;
        if result.deleted_count == 0 {
            return Err(AppError::NotFound(format!("account {id}")));
        }
        Ok(())
    }

    pub async fn set_active(
        &self,
        id: ObjectId,
        active: bool,
        reason: Option<&str>,
    ) -> Result<Account> {
        let mut set = doc! { "active": active, "updated_at": date(Utc::now()) };
        if active {
            set.insert("disabled_reason", Bson::Null);
            set.insert("consecutive_failures", 0i64);
            set.insert("last_error", Bson::Null);
        } else if let Some(reason) = reason {
            set.insert("disabled_reason", reason);
        }
        let update = doc! { "$set": set };
        self.update_account(id, update).await
    }

    pub async fn reset_usage(&self, account: &Account, now: DateTime<Utc>) -> Result<Account> {
        let anchor = account.window_anchor(now)?;
        let update = doc! {
            "$set": {
                "count_used": 0i64,
                "window_start": date(anchor),
                "rate_tokens": account.rate_limit_per_min.max(0) as f64,
                "rate_updated_at": date(now),
                "updated_at": date(now),
            }
        };
        self.update_account(account.id, update).await
    }

    /// Atomically reserve one send on `account`.
    ///
    /// Returns the updated account when a send was reserved, `None` when the
    /// account is out of quota, rate-limited, inactive or expired. The daily
    /// window rollover happens inside the same update, so a reservation right
    /// after the reset time never sees a stale counter.
    pub async fn reserve(&self, account: &Account, now: DateTime<Utc>) -> Result<Option<Account>> {
        let anchor = account.window_anchor(now)?;
        let now_b = date(now);
        let anchor_b = date(anchor);

        let stale = doc! {
            "$lt": [
                { "$ifNull": ["$window_start", anchor_b.clone()] },
                anchor_b.clone()
            ]
        };
        let used = doc! {
            "$cond": [ stale.clone(), Bson::Int64(0), { "$ifNull": ["$count_used", Bson::Int64(0)] } ]
        };
        let quota_unlimited = doc! {
            "$lte": [ { "$ifNull": ["$count_max", Bson::Int64(0)] }, Bson::Int64(0) ]
        };
        let quota_ok = doc! {
            "$or": [
                quota_unlimited.clone(),
                { "$lt": [ used.clone(), { "$ifNull": ["$count_max", Bson::Int64(0)] } ] }
            ]
        };
        let total_ok = doc! {
            "$or": [
                { "$lte": [ { "$ifNull": ["$total_max", Bson::Int64(0)] }, Bson::Int64(0) ] },
                { "$lt": [
                    { "$ifNull": ["$total_sent", Bson::Int64(0)] },
                    { "$ifNull": ["$total_max", Bson::Int64(0)] }
                ] }
            ]
        };
        let rate_unlimited = doc! {
            "$lte": [ { "$ifNull": ["$rate_limit_per_min", Bson::Int64(0)] }, Bson::Int64(0) ]
        };
        // Token bucket refill computed from persisted state, capped at one
        // minute of quota.
        let tokens = doc! {
            "$min": [
                { "$ifNull": ["$rate_limit_per_min", Bson::Int64(0)] },
                { "$add": [
                    { "$ifNull": ["$rate_tokens", Bson::Double(0.0)] },
                    { "$multiply": [
                        { "$divide": [
                            { "$subtract": [ now_b.clone(), { "$ifNull": ["$rate_updated_at", now_b.clone()] } ] },
                            Bson::Double(1000.0)
                        ] },
                        { "$divide": [
                            { "$ifNull": ["$rate_limit_per_min", Bson::Int64(0)] },
                            Bson::Double(60.0)
                        ] }
                    ] }
                ] }
            ]
        };

        let filter = doc! {
            "_id": account.id,
            "active": true,
            "$expr": { "$and": [
                quota_ok,
                total_ok,
                { "$or": [
                    { "$eq": [ { "$ifNull": ["$expires_at", Bson::Null] }, Bson::Null ] },
                    { "$gt": ["$expires_at", now_b.clone()] }
                ] },
                { "$or": [
                    rate_unlimited.clone(),
                    { "$gte": [ tokens.clone(), Bson::Double(1.0) ] }
                ] }
            ] }
        };

        let pipeline = vec![doc! {
            "$set": {
                "count_used": { "$add": [ used, Bson::Int64(1) ] },
                "window_start": { "$cond": [ stale, anchor_b, "$window_start" ] },
                "rate_tokens": { "$cond": [
                    rate_unlimited.clone(),
                    "$rate_tokens",
                    { "$subtract": [ tokens, Bson::Double(1.0) ] }
                ] },
                "rate_updated_at": { "$cond": [
                    rate_unlimited,
                    "$rate_updated_at",
                    now_b.clone()
                ] },
                "last_used_at": now_b.clone(),
                "updated_at": now_b,
            }
        }];

        let updated = self
            .accounts()
            .find_one_and_update(filter, pipeline)
            .return_document(ReturnDocument::After)
            .await?;
        if updated.is_none() {
            debug!(account = %account.name, "reservation rejected");
        }
        Ok(updated)
    }

    /// Refund a reservation and record a send failure.
    pub async fn mark_failure(
        &self,
        id: ObjectId,
        error: &str,
        fail_threshold: u32,
    ) -> Result<Account> {
        let now_b = date(Utc::now());
        let threshold = i64::from(fail_threshold);
        let auto_disable = fail_threshold > 0;
        let pipeline = vec![
            doc! { "$set": {
                "count_used": { "$max": [
                    Bson::Int64(0),
                    { "$subtract": [ { "$ifNull": ["$count_used", Bson::Int64(0)] }, Bson::Int64(1) ] }
                ] },
                "consecutive_failures": { "$add": [
                    { "$ifNull": ["$consecutive_failures", Bson::Int64(0)] },
                    Bson::Int64(1)
                ] },
                "last_error": error,
                "updated_at": now_b.clone(),
            } },
            doc! { "$set": {
                "active": { "$cond": [
                    { "$and": [
                        Bson::Boolean(auto_disable),
                        { "$gte": ["$consecutive_failures", Bson::Int64(threshold)] }
                    ] },
                    Bson::Boolean(false),
                    "$active"
                ] },
                "disabled_reason": { "$cond": [
                    { "$and": [
                        Bson::Boolean(auto_disable),
                        { "$gte": ["$consecutive_failures", Bson::Int64(threshold)] }
                    ] },
                    "auto-disabled after repeated send failures",
                    { "$ifNull": ["$disabled_reason", Bson::Null] }
                ] }
            } },
        ];
        let updated = self
            .accounts()
            .find_one_and_update(doc! { "_id": id }, pipeline)
            .return_document(ReturnDocument::After)
            .await?;
        updated.ok_or_else(|| AppError::NotFound(format!("account {id}")))
    }

    pub async fn mark_success(&self, id: ObjectId) -> Result<()> {
        let now_b = date(Utc::now());
        self.accounts()
            .update_one(
                doc! { "_id": id },
                doc! {
                    "$inc": { "total_sent": 1i64 },
                    "$set": {
                        "consecutive_failures": 0i64,
                        "last_error": Bson::Null,
                        "last_success_at": now_b.clone(),
                        "updated_at": now_b,
                    }
                },
            )
            .await?;
        Ok(())
    }

    pub async fn log_send(&self, log: &SendLog) -> Result<()> {
        self.logs().insert_one(log).await?;
        Ok(())
    }

    /// Reset stale quota windows and disable expired accounts.
    pub async fn maintenance(&self, now: DateTime<Utc>) -> Result<MaintenanceReport> {
        let mut report = MaintenanceReport::default();
        for account in self.list_accounts(false).await? {
            if account.active && account.is_expired(now) {
                let reason = match account.expires_at {
                    Some(at) => format!(
                        "expired at {}",
                        at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    ),
                    None => "expired".to_string(),
                };
                self.set_active(account.id, false, Some(&reason)).await?;
                report.accounts_disabled += 1;
                continue;
            }
            let anchor = match account.window_anchor(now) {
                Ok(anchor) => anchor,
                Err(e) => {
                    debug!(account = %account.name, error = %e, "cannot compute quota window");
                    continue;
                }
            };
            if anchor > account.window_start && account.count_used != 0 {
                let result = self
                    .accounts()
                    .update_one(
                        doc! { "_id": account.id, "window_start": { "$lt": date(anchor) } },
                        doc! { "$set": {
                            "count_used": 0i64,
                            "window_start": date(anchor),
                            "updated_at": date(now),
                        } },
                    )
                    .await?;
                if result.modified_count > 0 {
                    report.windows_reset += 1;
                }
            }
        }
        Ok(report)
    }

    pub async fn stats(&self, now: DateTime<Utc>) -> Result<Stats> {
        let accounts = self.list_accounts(false).await?;
        let mut stats = Stats {
            total_accounts: accounts.len() as u64,
            ..Stats::default()
        };
        for account in &accounts {
            if account.is_expired(now) {
                stats.expired_accounts += 1;
            } else if !account.active {
                stats.disabled_accounts += 1;
            } else {
                stats.active_accounts += 1;
            }
            if account.active && !account.is_expired(now) {
                if account.count_max > 0 {
                    stats.used_current_window += account.count_used.max(0);
                    stats.capacity_current_window += account.count_max;
                } else {
                    stats.unlimited_accounts += 1;
                }
            }
        }
        let since = now - TimeDelta::days(1);
        stats.sent_24h = self
            .logs()
            .count_documents(doc! { "ts": { "$gte": date(since) }, "status": "sent" })
            .await?;
        stats.failed_24h = self
            .logs()
            .count_documents(doc! { "ts": { "$gte": date(since) }, "status": "failed" })
            .await?;
        Ok(stats)
    }
}

fn date(value: DateTime<Utc>) -> Bson {
    Bson::DateTime(bson::DateTime::from(value))
}

fn map_write_error(error: mongodb::error::Error, kind: &str, key: &str) -> AppError {
    if let ErrorKind::Write(WriteFailure::WriteError(ref write_error)) = *error.kind {
        if write_error.code == 11000 {
            return AppError::Conflict(format!("{kind} `{key}` already exists"));
        }
    }
    AppError::Mongo(error)
}
