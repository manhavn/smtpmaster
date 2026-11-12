//! Inbound identities: one inbound account owns a pool of outbound accounts.
//!
//! An external app authenticates with a route's username/password and its mail
//! is only ever routed through that route's accounts, keeping tenants isolated.

use bson::oid::ObjectId;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Route {
    #[serde(rename = "_id")]
    pub id: ObjectId,
    /// Unique, human-friendly name used by the CLI.
    pub name: String,
    /// Inbound SMTP AUTH username used by the external app.
    pub username: String,
    pub password: String,
    #[serde(default)]
    pub description: Option<String>,
    pub active: bool,
    /// Outbound accounts this route may send through.
    #[serde(default)]
    pub account_ids: Vec<ObjectId>,
    #[serde(with = "crate::serde_bson::datetime")]
    pub created_at: DateTime<Utc>,
    #[serde(with = "crate::serde_bson::datetime")]
    pub updated_at: DateTime<Utc>,
}

impl Route {
    pub fn new(
        name: impl Into<String>,
        username: impl Into<String>,
        password: impl Into<String>,
        account_ids: Vec<ObjectId>,
        now: DateTime<Utc>,
    ) -> Self {
        Self {
            id: ObjectId::new(),
            name: name.into(),
            username: username.into(),
            password: password.into(),
            description: None,
            active: true,
            account_ids,
            created_at: now,
            updated_at: now,
        }
    }

    pub fn status_label(&self) -> &'static str {
        if self.active { "active" } else { "off" }
    }

    pub fn redacted(&self) -> Self {
        let mut copy = self.clone();
        copy.password = "***".to_string();
        copy
    }
}
