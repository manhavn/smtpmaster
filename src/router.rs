//! Routes each incoming message to one account with remaining capacity.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use mongodb::bson::oid::ObjectId;
use tokio::sync::Semaphore;
use tracing::warn;

use crate::config::{SelectionMode, ServerConfig};
use crate::error::{AppError, Result};
use crate::model::Account;
use crate::smtp::{self, protocol};
use crate::store::{SendLog, Store};

#[derive(Debug, Clone)]
pub struct Delivery {
    pub account_name: String,
    pub message_id: ObjectId,
    pub attempts: u32,
}

pub struct Router {
    store: Arc<Store>,
    mode: SelectionMode,
    max_attempts: usize,
    fail_threshold: u32,
    timeout: Duration,
    hostname: String,
    rewrite_from_header: bool,
    permits: Arc<Semaphore>,
    /// One permit per account so a single account never sends two messages at
    /// once; idle accounts are used in parallel instead.
    account_slots: Mutex<HashMap<ObjectId, Arc<Semaphore>>>,
}

impl Router {
    pub fn new(store: Arc<Store>, config: &ServerConfig) -> Self {
        Self {
            store,
            mode: config.mode,
            max_attempts: config.max_attempts,
            fail_threshold: config.fail_threshold,
            timeout: Duration::from_secs(config.send_timeout_secs.max(1)),
            hostname: config.hostname.clone(),
            rewrite_from_header: config.rewrite_from_header,
            permits: Arc::new(Semaphore::new(if config.sequential {
                1
            } else {
                config.concurrency.max(1)
            })),
            account_slots: Mutex::new(HashMap::new()),
        }
    }

    fn account_slot(&self, id: ObjectId) -> Arc<Semaphore> {
        let mut slots = self
            .account_slots
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        Arc::clone(
            slots
                .entry(id)
                .or_insert_with(|| Arc::new(Semaphore::new(1))),
        )
    }

    pub async fn dispatch(
        &self,
        route_id: Option<ObjectId>,
        mail_from: &str,
        recipients: &[String],
        raw: &[u8],
    ) -> Result<Delivery> {
        if recipients.is_empty() {
            return Err(AppError::Invalid("no recipients".into()));
        }
        let allowed = match route_id {
            Some(id) => {
                let route = self.store.find_route_by_id(id).await?;
                if !route.active {
                    return Err(AppError::Invalid("route is disabled".into()));
                }
                if route.account_ids.is_empty() {
                    return Err(AppError::NoCapacity);
                }
                Some(route.account_ids.into_iter().collect::<HashSet<ObjectId>>())
            }
            None => None,
        };
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| AppError::Internal("send semaphore closed".into()))?;
        let now = Utc::now();
        let candidates = self.candidates(now, allowed.as_ref()).await?;
        if candidates.is_empty() {
            return Err(AppError::NoCapacity);
        }

        let mut attempts = 0u32;
        let mut last_error = None;
        let attempt_limit = if self.max_attempts == 0 {
            usize::MAX
        } else {
            self.max_attempts
        };
        for account in candidates {
            if attempts as usize >= attempt_limit {
                break;
            }
            let slot = self.account_slot(account.id);
            let Ok(_slot_guard) = slot.try_acquire_owned() else {
                // The account is already sending for another app; take the
                // next idle account instead of queueing behind it.
                continue;
            };
            let Some(reserved) = self.store.reserve(&account, now).await? else {
                continue;
            };
            attempts += 1;
            let started = Instant::now();
            let envelope_from = if reserved.uses_account_sender() || mail_from.trim().is_empty() {
                reserved.from_address.clone()
            } else {
                mail_from.trim().to_string()
            };
            let body = if self.rewrite_from_header {
                protocol::rewrite_from_header(
                    raw,
                    reserved.from_name.as_deref(),
                    &reserved.from_address,
                )
            } else {
                raw.to_vec()
            };

            match smtp::send(
                &reserved,
                &envelope_from,
                recipients,
                &body,
                &self.hostname,
                self.timeout,
            )
            .await
            {
                Ok(()) => {
                    let _ = self.store.mark_success(reserved.id).await;
                    let log = SendLog {
                        id: ObjectId::new(),
                        account_id: reserved.id,
                        account_name: reserved.name.clone(),
                        from: envelope_from.clone(),
                        recipients: recipients.to_vec(),
                        status: "sent".to_string(),
                        error: None,
                        attempts,
                        duration_ms: started.elapsed().as_millis() as u64,
                        ts: Utc::now(),
                    };
                    let message_id = log.id;
                    let _ = self.store.log_send(&log).await;
                    return Ok(Delivery {
                        account_name: reserved.name,
                        message_id,
                        attempts,
                    });
                }
                Err(error) => {
                    let message = error.to_string();
                    warn!(account = %reserved.name, error = %message, "upstream send failed");
                    match self
                        .store
                        .mark_failure(reserved.id, &message, self.fail_threshold)
                        .await
                    {
                        Ok(updated) if !updated.active => warn!(
                            account = %reserved.name,
                            "account auto-disabled after repeated failures"
                        ),
                        Ok(_) => {}
                        Err(record_error) => warn!(
                            account = %reserved.name,
                            error = %record_error,
                            "could not record send failure"
                        ),
                    }
                    let log = SendLog {
                        id: ObjectId::new(),
                        account_id: reserved.id,
                        account_name: reserved.name.clone(),
                        from: envelope_from,
                        recipients: recipients.to_vec(),
                        status: "failed".to_string(),
                        error: Some(message),
                        attempts,
                        duration_ms: started.elapsed().as_millis() as u64,
                        ts: Utc::now(),
                    };
                    let _ = self.store.log_send(&log).await;
                    last_error = Some(error);
                }
            }
        }
        Err(last_error.unwrap_or(AppError::NoCapacity))
    }

    async fn candidates(
        &self,
        now: DateTime<Utc>,
        allowed: Option<&HashSet<ObjectId>>,
    ) -> Result<Vec<Account>> {
        let mut accounts: Vec<Account> = self
            .store
            .list_accounts(true)
            .await?
            .into_iter()
            .filter(|account| allowed.is_none_or(|ids| ids.contains(&account.id)))
            .filter(|account| account.is_available(now))
            .filter(|account| account.effective_priority(now).is_some())
            .collect();

        match self.mode {
            SelectionMode::Priority | SelectionMode::TimeWindow => accounts.sort_by(|a, b| {
                priority_of(a, now)
                    .cmp(&priority_of(b, now))
                    .then_with(|| a.last_used_at.cmp(&b.last_used_at))
                    .then_with(|| a.name.cmp(&b.name))
            }),
            SelectionMode::RoundRobin => accounts.sort_by(|a, b| {
                a.last_used_at
                    .cmp(&b.last_used_at)
                    .then_with(|| priority_of(a, now).cmp(&priority_of(b, now)))
                    .then_with(|| a.name.cmp(&b.name))
            }),
        }
        Ok(accounts)
    }
}

fn priority_of(account: &Account, now: DateTime<Utc>) -> i32 {
    account.effective_priority(now).unwrap_or(i32::MAX)
}
