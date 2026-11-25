//! After a selection is stored: its one delivery, then its result on the card and the thread
//! archived and locked. A pass may repeat the display; no input is ever sent again.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use poise::serenity_prelude as serenity;
use sqlx::PgPool;
use uuid::Uuid;

use super::card;
use crate::db::decisions::selection::{self, Ended, Selection};
use crate::services::discord::SharedData;
use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::discord::health::decision_target::{StartPath, deliver_once};
use crate::services::discord::health::{self, HealthRegistry};
use crate::services::provider::ProviderKind;

const PASS_EVERY: std::time::Duration = std::time::Duration::from_secs(15);

/// Shows `status` on the request's starter, its buttons enabled or not.
pub(crate) async fn show(
    pool: &PgPool,
    http: &serenity::Http,
    (request_id, thread): (Uuid, u64),
    status: &str,
    enabled: bool,
) -> Result<(), serenity::Error> {
    let buttons = selection::buttons(pool, request_id).await;
    let (Ok(buttons), true) = (buttons, thread != 0) else {
        return Err(serenity::Error::Other("decision starter unread"));
    };
    // A forum post's thread and starter message share one id.
    let (channel, message) = (
        serenity::ChannelId::new(thread),
        serenity::MessageId::new(thread),
    );
    let starter = http.get_message(channel, message).await?;
    let content = card::with_status(&starter.content, status);
    let rows = card::buttons(&buttons, enabled);
    let edited = crate::services::discord::http::edit_channel_message_with_components(
        http,
        channel,
        message,
        content.as_str(),
        rows,
    );
    edited.await.map(drop)
}

/// The selection's one delivery, to the session its requester's channel has now.
pub(crate) async fn deliver(pool: &PgPool, registry: &HealthRegistry, selection: &Selection) {
    let Some(provider) = ProviderKind::from_str(&selection.provider) else {
        return;
    };
    let normalizer = StartPath::new(registry, &provider, selection.logical_channel);
    let event = (selection.event_id, selection.text.as_str());
    let request_id = selection.request_id;
    match deliver_once(registry, pool, event, &provider, &normalizer).await {
        Ok(attempt) => tracing::info!(%request_id, ?attempt, "decision selection delivery"),
        Err(error) => tracing::warn!(%request_id, %error, "decision selection delivery stopped"),
    }
}

/// The runtime in this process that owns the selection's channel now, with its REST client.
async fn owner(
    registry: &HealthRegistry,
    selection: &Selection,
) -> Option<(Arc<serenity::Http>, ProviderKind)> {
    let provider = ProviderKind::from_str(&selection.provider)?;
    let channel = serenity::ChannelId::new(selection.logical_channel.max(1));
    let owner = health::resolve_direct_channel_runtime(registry, channel, &provider).await;
    Some((owner.ok()?.0, provider))
}

/// One pass: an unstarted selection is started once, attempts are settled from their evidence
/// without another write, and a request whose delivery ended is closed.
pub(crate) async fn converge(pool: &PgPool, registry: &HealthRegistry) {
    for selection in selection::unstarted(pool).await.unwrap_or_default() {
        // Only the process whose runtime owns the channel starts it; another leaves it be.
        if owner(registry, &selection).await.is_some() {
            deliver(pool, registry, &selection).await;
        }
    }
    if selection::delivering(pool).await.unwrap_or(false) {
        crate::services::discord::decision_drain::settle_abandoned(pool).await;
    }
    for ended in selection::ended(pool).await.unwrap_or_default() {
        close(pool, registry, &ended).await;
    }
}

fn refused(error: &serenity::Error) -> bool {
    let confirmed = |status: u16| matches!(status, 400 | 401 | 403 | 404);
    matches!(error, serenity::Error::Http(http)
        if http.status_code().is_some_and(|status| confirmed(status.as_u16())))
}

/// Closes the request in PG, shows the result and has the announce bot archive and lock the
/// thread. A display not made yet is tried by the next pass; a refused one is recorded.
async fn close(pool: &PgPool, registry: &HealthRegistry, ended: &Ended) {
    let (selection, request_id) = (&ended.selection, ended.selection.request_id);
    if let Err(error) = selection::close(pool, request_id, None).await {
        tracing::warn!(%request_id, %error, "decision request not closed yet");
        return;
    }
    let announce = health::resolve_utility_bot_http(registry, UtilityBotRole::Announce).await;
    let (Some((http, _)), Ok(announce)) = (owner(registry, selection).await, announce) else {
        return;
    };
    let post = (request_id, selection.thread_id);
    let thread = serenity::ChannelId::new(selection.thread_id.max(1));
    let status = card::ended(&selection.label, ended.state, ended.detail.as_deref());
    let sealed: Result<(), serenity::Error> = async {
        if show(pool, &http, post, &status, false).await.is_err() {
            // Archived or locked meanwhile: the announce bot makes the post editable first.
            let editable = serenity::builder::EditThread::new()
                .archived(false)
                .locked(false);
            thread.edit_thread(&announce, editable).await?;
            show(pool, &http, post, &status, false).await?;
        }
        let closed = serenity::builder::EditThread::new()
            .archived(true)
            .locked(true);
        thread.edit_thread(&announce, closed).await.map(drop)
    }
    .await;
    let outcome = match &sealed {
        Ok(()) => None,
        Err(error) if refused(error) => Some("moderation_refused"),
        Err(error) => {
            tracing::warn!(%request_id, %error, "decision thread not sealed yet");
            return;
        }
    };
    if let Err(error) = selection::close(pool, request_id, Some(outcome)).await {
        tracing::warn!(%request_id, %error, "decision seal not recorded");
    }
}

/// Starts this process's one pass loop, from the first runtime that has storage.
pub(crate) fn spawn_worker(shared: &Arc<SharedData>) {
    static STARTED: AtomicBool = AtomicBool::new(false);
    let Some(pool) = shared.pg_pool.clone() else {
        return;
    };
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    let shared = Arc::clone(shared);
    let pass = async move {
        loop {
            if let Some(registry) = shared.health_registry() {
                converge(&pool, &registry).await;
            }
            tokio::time::sleep(PASS_EVERY).await;
        }
    };
    crate::services::resident_loops::spawn_resident("decision_selection_pass", pass);
}
