//! Background quota reset / expiry sweep.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::time::MissedTickBehavior;
use tracing::{info, warn};

use crate::store::Store;

pub async fn watch(store: Arc<Store>, interval: Duration) {
    let mut ticker = tokio::time::interval(interval.max(Duration::from_secs(1)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        match store.maintenance(Utc::now()).await {
            Ok(report) => {
                if report.windows_reset > 0 || report.accounts_disabled > 0 {
                    info!(
                        windows_reset = report.windows_reset,
                        accounts_disabled = report.accounts_disabled,
                        "maintenance pass complete"
                    );
                }
            }
            Err(error) => warn!(error = %error, "maintenance pass failed"),
        }
    }
}
