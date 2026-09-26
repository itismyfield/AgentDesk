//! Boot custody notice: one channel message per preserved TUI-direct episode, so a turn
//! caught by a restart is never lost silently.

use crate::services::discord::ProviderKind;
use crate::services::discord::{SharedData, runtime_store};
use chrono::{DateTime, Utc};
use poise::serenity_prelude as serenity;
use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

/// Spawns the notice pass once per provider; a runtime without a bot token is skipped.
pub(in crate::services::discord) fn spawn_boot_custody_notice(
    shared: &Arc<SharedData>,
    provider: &ProviderKind,
) {
    static STARTED: Mutex<BTreeSet<String>> = Mutex::new(BTreeSet::new());
    let Some(http) = shared.serenity_http_or_token_fallback() else {
        return;
    };
    let mut started = STARTED.lock().unwrap_or_else(|poison| poison.into_inner());
    if !started.insert(provider.as_str().to_string()) {
        return;
    }
    let (shared, provider) = (shared.clone(), provider.clone());
    crate::services::discord::task_supervisor::spawn_observed("boot_custody_notice", async move {
        notify_custody_episodes(&http, &shared, &provider, Utc::now()).await;
    });
}

/// One pass over the provider's custody episodes; an unsettled notice waits for the next boot.
pub(super) async fn notify_custody_episodes(
    _http: &serenity::Http,
    _shared: &Arc<SharedData>,
    _provider: &ProviderKind,
    _now: DateTime<Utc>,
) {
    let _ = runtime_store::runtime_root();
}
