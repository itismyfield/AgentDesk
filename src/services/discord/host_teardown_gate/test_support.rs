//! Runtimes, sessions rows, markers and busy turns the host-guard caller tests seed.

use std::sync::Arc;

use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use sqlx::PgPool;

use crate::db::dispatched_sessions::hosted_execution::HostedState;
use crate::db::dispatched_sessions::hosted_execution::tests::{future_schema, owner, record, wire};
use crate::services::discord::health::HealthRegistry;
use crate::services::discord::{SharedData, inflight};
use crate::services::provider::cancel_token_cleanup::authority::TmuxBinding;
use crate::services::provider::{CancelToken, ProviderKind};

/// What the stored rows say about one tmux session before a teardown reads them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stored {
    /// A found sessions row with no hosted record: the only legacy answer.
    Legacy,
    /// A row carrying a bound Herdr record.
    Hosted,
    /// A record in a schema this build cannot read.
    Future,
    /// No sessions row and no other trace.
    Missing,
    /// No sessions row, but the `.host_kind` marker says Herdr.
    MissingHerdrMarker,
    /// A legacy row whose `.host_kind` marker says Herdr.
    LegacyHerdrMarker,
}

impl Stored {
    pub(crate) const ALL: [Self; 6] = [
        Self::Legacy,
        Self::Hosted,
        Self::Future,
        Self::Missing,
        Self::MissingHerdrMarker,
        Self::LegacyHerdrMarker,
    ];
}

/// A claude runtime on `pool`.
pub(crate) async fn shared_on(pool: &PgPool) -> Arc<SharedData> {
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    shared.settings.write().await.provider = ProviderKind::Claude;
    shared
}

/// [`shared_on`], registered for the registry-keyed callers.
pub(crate) async fn runtime(pool: &PgPool) -> (Arc<SharedData>, Arc<HealthRegistry>) {
    let shared = shared_on(pool).await;
    let registry = Arc::new(HealthRegistry::new());
    registry
        .register("claude".to_string(), shared.clone())
        .await;
    (shared, registry)
}

/// Limits `shared` to the `allowed` channels; an empty list lets it take any channel.
pub(crate) async fn allow_channels(shared: &SharedData, allowed: &[u64]) {
    shared.settings.write().await.allowed_channel_ids = allowed.to_vec();
}

/// The key the channel's turns write for `tmux_name`.
pub(crate) fn channel_key(shared: &SharedData, tmux_name: &str) -> String {
    let provider = ProviderKind::Claude;
    crate::services::discord::adk_session::build_namespaced_session_key(
        &shared.token_hash,
        &provider,
        tmux_name,
    )
}

/// Seeds `stored` for `tmux_name` under `key` on `channel_id`.
pub(crate) async fn seed(
    pool: &PgPool,
    key: &str,
    tmux_name: &str,
    channel_id: u64,
    stored: Stored,
) {
    let bound = || {
        wire(&record(
            &owner(&channel_id.to_string()),
            "n1",
            HostedState::Bound,
        ))
    };
    let raw = match stored {
        Stored::Legacy | Stored::LegacyHerdrMarker => Some(None),
        Stored::Hosted => Some(Some(bound())),
        Stored::Future => Some(Some(future_schema(&owner(&channel_id.to_string())))),
        Stored::Missing | Stored::MissingHerdrMarker => None,
    };
    if let Some(raw) = raw {
        inflight::seed_session_row_keyed(pool, key, channel_id, raw).await;
    }
    if matches!(
        stored,
        Stored::MissingHerdrMarker | Stored::LegacyHerdrMarker
    ) {
        let marker = crate::services::tmux_common::session_temp_path(tmux_name, "host_kind");
        std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
        std::fs::write(marker, "herdr").unwrap();
    }
}

/// A busy mailbox turn on `channel_id` whose token and inflight row name `tmux_name`.
pub(crate) async fn busy_turn(
    shared: &SharedData,
    channel_id: ChannelId,
    tmux_name: &str,
) -> Arc<CancelToken> {
    let user_msg = MessageId::new(channel_id.get() + 1);
    let token = Arc::new(CancelToken::new());
    *token.tmux_binding.lock().unwrap() = Some(TmuxBinding::NameOnly {
        name: tmux_name.to_string(),
    });
    let started = crate::services::discord::mailbox_try_start_turn(
        shared,
        channel_id,
        token.clone(),
        UserId::new(7),
        user_msg,
    );
    assert!(started.await, "the seeded turn must start");
    let row = inflight::InflightTurnState::new(
        ProviderKind::Claude,
        channel_id.get(),
        None,
        1,
        user_msg.get(),
        user_msg.get() + 1,
        "host guard caller fixture".to_string(),
        None,
        Some(tmux_name.to_string()),
        None,
        None,
        0,
    );
    inflight::save_inflight_state_create_new(&row).expect("persist the inflight row");
    token
}

/// Whether the seeded turn still owns its mailbox, uncancelled, with its inflight row.
pub(crate) async fn turn_kept(
    shared: &SharedData,
    channel_id: ChannelId,
    token: &CancelToken,
) -> bool {
    let snapshot = crate::services::discord::mailbox_snapshot(shared, channel_id).await;
    !token.cancelled.load(std::sync::atomic::Ordering::Relaxed)
        && snapshot.active_user_message_id == Some(MessageId::new(channel_id.get() + 1))
        && inflight::load_inflight_state(&ProviderKind::Claude, channel_id.get()).is_some()
}

/// Whether a turn stop was recorded for `channel_id`, the first change a force-kill makes.
#[cfg(unix)]
pub(crate) fn stop_recorded(channel_id: ChannelId) -> bool {
    crate::services::discord::tmux::recent_turn_stop_for_channel(channel_id).is_some()
}

/// A busy mailbox turn on `channel_id` whose token holds no tmux binding and which stores no
/// inflight row: the shape of a process-backend turn.
pub(crate) async fn nameless_turn(shared: &SharedData, channel_id: ChannelId) -> Arc<CancelToken> {
    let token = Arc::new(CancelToken::new());
    let user_msg = MessageId::new(channel_id.get() + 1);
    let start = crate::services::discord::mailbox_try_start_turn;
    assert!(start(shared, channel_id, token.clone(), UserId::new(7), user_msg).await);
    token
}

/// Whether the seeded turn still owns `channel_id`'s mailbox.
pub(crate) async fn mailbox_turn_active(shared: &SharedData, channel_id: ChannelId) -> bool {
    let snapshot = crate::services::discord::mailbox_snapshot(shared, channel_id).await;
    snapshot.active_user_message_id == Some(MessageId::new(channel_id.get() + 1))
}

/// Rewrites the channel's inflight row as an older build stored it, with no finalizer id, so
/// the normal inflight load backfills and saves it again. Returns the file path.
pub(crate) fn inflight_needing_backfill(channel_id: ChannelId) -> std::path::PathBuf {
    let root = inflight::inflight_runtime_root().expect("inflight root");
    let path = inflight::inflight_state_path(&root, &ProviderKind::Claude, channel_id.get());
    let raw = std::fs::read_to_string(&path).expect("seeded inflight row");
    let mut raw: serde_json::Value = serde_json::from_str(&raw).unwrap();
    raw.as_object_mut().unwrap().remove("finalizer_turn_id");
    std::fs::write(&path, raw.to_string()).unwrap();
    path
}

/// Makes `shared` run `tmux_name` on `channel_id` (watcher, session, with `busy` a turn); with
/// `hosted`, the channel's row under the runtime's hash is a bound Herdr record for that name.
pub(crate) async fn running_session(
    shared: &SharedData,
    pool: &PgPool,
    channel_id: ChannelId,
    tmux_name: &str,
    busy: bool,
    hosted: Option<&str>,
) {
    if busy {
        busy_turn(shared, channel_id, tmux_name).await;
    }
    crate::services::discord::register_resume_watcher_for_tests(shared, channel_id, tmux_name);
    let base = tmux_name
        .strip_prefix("AgentDesk-claude-")
        .expect("a claude name");
    let session = crate::services::discord::DiscordSession {
        session_id: Some(format!("{base}-sid")),
        memento_context_loaded: false,
        memento_reflected: false,
        current_path: None,
        history: Vec::new(),
        pending_uploads: Vec::new(),
        cleared: false,
        remote_profile_name: None,
        channel_id: Some(channel_id.get()),
        channel_name: Some(base.to_string()),
        category_name: None,
        last_active: tokio::time::Instant::now(),
        worktree: None,
        born_generation: 0,
    };
    shared
        .core
        .lock()
        .await
        .sessions
        .insert(channel_id, session);
    let Some(hosted) = hosted else {
        return;
    };
    let (key, channel) = (channel_key(shared, hosted), channel_id.get());
    let bound = wire(&record(
        &owner(&channel.to_string()),
        "n1",
        HostedState::Bound,
    ));
    let hash = &shared.token_hash;
    inflight::seed_session_row_hashed(pool, &key, channel, hash, Some(bound)).await;
}

/// Everything a stop on `channel_id` changes in `shared`: the mailbox turn and its token, the
/// active-turn counter, the watcher, the session's provider id and the inflight row's bytes.
pub(crate) async fn runtime_state(shared: &SharedData, channel_id: ChannelId) -> String {
    use std::sync::atomic::Ordering::SeqCst;
    let snapshot = crate::services::discord::mailbox_snapshot(shared, channel_id).await;
    let token = snapshot.cancel_token.as_ref();
    let cancelled = token.map(|token| token.cancelled.load(SeqCst));
    let active = shared.restart.global_active.load(SeqCst);
    let watcher = shared.tmux_watchers.channel_binding(&channel_id);
    let watcher = watcher.map(|binding| binding.tmux_session_name);
    let data = shared.core.lock().await;
    let session = data.sessions.get(&channel_id);
    let session = session.map(|session| (session.session_id.clone(), session.channel_name.clone()));
    let root = inflight::inflight_runtime_root().expect("inflight root");
    let path = inflight::inflight_state_path(&root, &ProviderKind::Claude, channel_id.get());
    let bytes = std::fs::read(path).ok();
    let mailbox = (snapshot.active_user_message_id, cancelled);
    format!("{:?}", (mailbox, active, watcher, session, bytes))
}
