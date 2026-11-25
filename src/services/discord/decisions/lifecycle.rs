//! After a selection is stored: its one delivery, then its result on the card and the thread
//! archived and locked. A pass may repeat the display; no input is ever sent again.

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use poise::serenity_prelude as serenity;
use sqlx::PgPool;
use uuid::Uuid;

use super::card;
use crate::db::decisions::selection::{self, Selection};
use crate::db::postgres::PooledAdvisoryLock;
use crate::services::discord::SharedData;
use crate::services::discord::bot_role::UtilityBotRole;
use crate::services::discord::health::decision_target::{StartPath, deliver_once};
use crate::services::discord::health::{self, HealthRegistry};
use crate::services::provider::ProviderKind;

const PASS_EVERY: std::time::Duration = std::time::Duration::from_secs(15);
const CARD_LOCK: &str = "decision_card";

/// Runs `edit` holding the request's card lock. Every card edit reads the request's lifecycle
/// inside it, so edits apply in lifecycle order; the lock's session holds no transaction.
pub(crate) async fn under_card_lock<T>(
    pool: &PgPool,
    request_id: Uuid,
    edit: impl Future<Output = T>,
) -> Result<T, sqlx::Error> {
    let lock = PooledAdvisoryLock::acquire(pool, CARD_LOCK, &request_id.to_string()).await?;
    let done = edit.await;
    if let Err(error) = lock.unlock().await {
        tracing::warn!(%error, "decision card lock release failed; the session end frees it");
    }
    Ok(done)
}

/// Edits `status` and the buttons onto the request's starter; the caller holds the card lock.
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

/// Shows the stored choice as being delivered, unless the request already moved on.
pub(crate) async fn show_selecting(pool: &PgPool, http: &serenity::Http, request_id: Uuid) {
    let shown = under_card_lock(pool, request_id, async {
        let card = selection::card(pool, request_id).await.ok().flatten();
        let Some(card) = card.filter(|card| card.lifecycle == "selecting") else {
            return Ok(());
        };
        let label = card.chosen.as_ref().map_or("", |chosen| chosen.0.as_str());
        let post = (request_id, card.thread_id);
        show(pool, http, post, &card::selecting(label), false).await
    });
    match shown.await {
        Ok(Ok(())) => {}
        Ok(Err(error)) => tracing::warn!(%error, "decision card still shows open buttons"),
        Err(error) => tracing::warn!(%error, "decision card lock not taken"),
    }
}

/// The runtime in this process that owns the selection's channel now.
struct Owner {
    /// Its REST client, enough for a card edit; a token alone gives one.
    http: Arc<serenity::Http>,
    provider: ProviderKind,
    /// Its gateway is up with its Context, which a start needs.
    ready: bool,
}

async fn owner(registry: &HealthRegistry, selection: &Selection) -> Option<Owner> {
    let provider = ProviderKind::from_str(&selection.provider)?;
    let channel = serenity::ChannelId::new(selection.logical_channel.max(1));
    let owner = health::resolve_direct_channel_runtime(registry, channel, &provider).await;
    let (http, shared) = owner.ok()?;
    let ready = shared.http.cached_serenity_ctx.get().is_some()
        && shared.bot_connected.load(Ordering::SeqCst);
    Some(Owner {
        http,
        provider,
        ready,
    })
}

/// The selection's one delivery, to the session its requester's channel has now. A process that
/// cannot start it yet takes no attempt: the event stays unstarted for a pass that can.
pub(crate) async fn deliver(pool: &PgPool, registry: &HealthRegistry, selection: &Selection) {
    let Some(owner) = owner(registry, selection).await.filter(|owner| owner.ready) else {
        return;
    };
    let provider = &owner.provider;
    let normalizer = StartPath::new(registry, provider, selection.logical_channel);
    let event = (selection.event_id, selection.text.as_str());
    let request_id = selection.request_id;
    match deliver_once(registry, pool, event, provider, &normalizer).await {
        Ok(attempt) => tracing::info!(%request_id, ?attempt, "decision selection delivery"),
        Err(error) => tracing::warn!(%request_id, %error, "decision selection delivery stopped"),
    }
}

/// One pass: an unstarted selection is started once, attempts are settled from their evidence
/// without another write, and a request whose delivery ended is closed.
pub(crate) async fn converge(pool: &PgPool, registry: &HealthRegistry) {
    for selection in selection::unstarted(pool).await.unwrap_or_default() {
        deliver(pool, registry, &selection).await;
    }
    if selection::delivering(pool).await.unwrap_or(false) {
        crate::services::discord::decision_drain::settle_abandoned(pool).await;
    }
    for ended in selection::ended(pool).await.unwrap_or_default() {
        let closed = under_card_lock(pool, ended.selection.request_id, async {
            close(pool, registry, &ended.selection).await
        });
        if let Err(error) = closed.await {
            tracing::warn!(%error, "decision card lock not taken; the next pass closes it");
        }
    }
}

pub(crate) fn refused(error: &serenity::Error) -> bool {
    let confirmed = |status: u16| matches!(status, 400 | 401 | 403 | 404);
    matches!(error, serenity::Error::Http(http)
        if http.status_code().is_some_and(|status| confirmed(status.as_u16())))
}

/// Under the card lock: closes the request in PG, shows the end PG holds now and has the
/// announce bot archive and lock the thread. A refused display is recorded, another is retried.
async fn close(pool: &PgPool, registry: &HealthRegistry, selection: &Selection) {
    use crate::db::decisions::AttemptState::{Delivered, Failed, UnknownClosed};
    let request_id = selection.request_id;
    let card = selection::card(pool, request_id).await.ok().flatten();
    let Some(card) = card.filter(|card| !card.sealed) else {
        return;
    };
    let Some((label, state @ (Delivered | Failed | UnknownClosed), detail)) = &card.chosen else {
        return;
    };
    if let Err(error) = selection::close(pool, request_id, None).await {
        tracing::warn!(%request_id, %error, "decision request not closed yet");
        return;
    }
    let announce = health::resolve_utility_bot_http(registry, UtilityBotRole::Announce).await;
    let (Some(owner), Ok(announce)) = (owner(registry, selection).await, announce) else {
        return;
    };
    let post = (request_id, card.thread_id);
    let thread = serenity::ChannelId::new(card.thread_id.max(1));
    let status = card::ended(label, *state, detail.as_deref());
    let sealed: Result<(), serenity::Error> = async {
        if show(pool, &owner.http, post, &status, false).await.is_err() {
            // Archived or locked meanwhile: the announce bot makes the post editable first.
            let editable = serenity::builder::EditThread::new()
                .archived(false)
                .locked(false);
            thread.edit_thread(&announce, editable).await?;
            show(pool, &owner.http, post, &status, false).await?;
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
