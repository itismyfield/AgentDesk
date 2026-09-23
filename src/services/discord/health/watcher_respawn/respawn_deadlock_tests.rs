//! Force-clean watcher respawn against a TUI-direct inflight row that has no
//! owning watcher. Every test drives the production rebind / cleaner / row
//! writers in an isolated runtime root with a fake `tmux` on PATH and a Discord
//! HTTP client proxied to a closed local port, so nothing leaves the process.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Instant;

use poise::serenity_prelude::{self as serenity, ChannelId, MessageId, UserId};

use super::super::HealthRegistry;
use super::{
    force_clean_should_respawn_watcher, release_stale_mailbox_ownership_after_force_clean,
};
use crate::config::TestEnvVarGuard;
use crate::config::test_env_lock::{SharedTestEnvLockGuard, acquire_shared_test_env_lock};
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::inflight::{
    self, GuardedSaveOutcome, InflightEpisodePin, InflightTurnIdentity, InflightTurnState,
    OrphanRelayReclaimOutcome, RelayOwnerKind,
};
use crate::services::discord::recovery_engine::RebindError;
use crate::services::discord::{self as discord, SharedData};
use crate::services::provider::{CancelToken, ProviderKind};
use crate::services::tui_prompt_dedupe::{
    EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED, ExternalInputRelayLease,
    ExternalInputRelayOwner,
};

const PROVIDER: ProviderKind = ProviderKind::Claude;
const INJECTED_PROMPT_MSG_ID: u64 = 6_159_000_001;
const MAILBOX_OWNER_USER_ID: u64 = 11;

/// Runtime root, fake-tmux PATH and the env mutex as one unit. Field order is
/// drop order: both vars are restored before the mutex is released.
struct Fixture {
    root: tempfile::TempDir,
    _root_env: TestEnvVarGuard,
    _path_env: TestEnvVarGuard,
    _tmux_dir: tempfile::TempDir,
    _lock: SharedTestEnvLockGuard,
}

impl Fixture {
    /// `tmux_alive` decides how every liveness probe answers.
    fn new(tmux_alive: bool) -> Self {
        use std::os::unix::fs::PermissionsExt;

        let lock = acquire_shared_test_env_lock();
        let root = tempfile::tempdir().expect("runtime root");
        let root_env =
            TestEnvVarGuard::set_path_after_shared_test_env_lock("AGENTDESK_ROOT_DIR", root.path());

        let tmux_dir = tempfile::tempdir().expect("fake tmux dir");
        let body = if tmux_alive {
            "case \"$1\" in list-panes) echo 0 ;; esac; exit 0"
        } else {
            "echo \"can't find session: test\" >&2; exit 1"
        };
        let script = tmux_dir.path().join("tmux");
        std::fs::write(
            &script,
            format!("#!/bin/sh\nwhile [ \"${{1#-}}\" != \"$1\" ]; do shift; done\n{body}\n"),
        )
        .expect("write fake tmux");
        let mut perms = std::fs::metadata(&script).expect("stat").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&script, perms).expect("chmod");
        let joined = std::env::join_paths(std::iter::once(tmux_dir.path().to_path_buf()).chain(
            std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()),
        ))
        .expect("join PATH");
        let path_env = TestEnvVarGuard::set_path_after_shared_test_env_lock(
            "PATH",
            std::path::Path::new(joined.as_os_str()),
        );

        Self {
            root,
            _root_env: root_env,
            _path_env: path_env,
            _tmux_dir: tmux_dir,
            _lock: lock,
        }
    }

    fn row_path(&self, channel: ChannelId) -> std::path::PathBuf {
        inflight::inflight_state_path(
            &self.root.path().join("runtime").join("discord_inflight"),
            &PROVIDER,
            channel.get(),
        )
    }
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("test runtime")
        .block_on(future)
}

/// Discord client whose requests go to a local port nobody listens on.
fn offline_http() -> Arc<serenity::Http> {
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .expect("reserve local port")
        .port();
    Arc::new(
        serenity::HttpBuilder::new("test-token")
            .proxy(format!("http://127.0.0.1:{port}"))
            .ratelimiter_disabled(true)
            .build(),
    )
}

/// A single runtime registered the way dcserver does, so the respawn resolves it.
async fn registry_with(shared: &Arc<SharedData>) -> HealthRegistry {
    let registry = HealthRegistry::new();
    registry
        .register(PROVIDER.as_str().to_string(), shared.clone())
        .await;
    registry
        .register_http(PROVIDER.as_str().to_string(), offline_http())
        .await;
    registry
}

/// The row the TUI-direct claim writes when no watcher can own the turn.
fn watcherless_tui_direct_row(channel: ChannelId, tmux: &str) -> InflightTurnState {
    let lease = ExternalInputRelayLease {
        channel_id: Some(channel.get()),
        turn_id: Some(format!("external:claude:{}:tmux:1", channel.get())),
        session_key: Some(format!("token:{tmux}")),
        relay_owner: ExternalInputRelayOwner::SessionBoundRelay,
        runtime_kind: Some(RuntimeHandoffKind::ClaudeTui),
        generation: EXTERNAL_INPUT_RELAY_LEASE_GENERATION_UNRECORDED,
    };
    discord::tui_prompt_relay::synthetic_start::build_tui_direct_synthetic_inflight_state(
        PROVIDER,
        channel,
        MessageId::new(INJECTED_PROMPT_MSG_ID),
        None,
        "typed in the TUI",
        tmux,
        None,
        0,
        &lease,
        RelayOwnerKind::SessionBoundRelay,
    )
}

fn claim_row(row: &InflightTurnState) {
    assert!(
        inflight::save_inflight_state_if_absent(row).expect("claim inflight row"),
        "fixture: the claim must land on an empty store"
    );
}

async fn engage_mailbox(shared: &SharedData, channel: ChannelId) {
    assert!(
        discord::mailbox_try_start_turn(
            shared,
            channel,
            Arc::new(CancelToken::new()),
            UserId::new(MAILBOX_OWNER_USER_ID),
            MessageId::new(INJECTED_PROMPT_MSG_ID),
        )
        .await,
        "fixture: the mailbox must accept the turn"
    );
}

/// Force-clean's release step, fed the owner captured at the stall snapshot.
async fn force_clean_release(shared: &Arc<SharedData>, channel: ChannelId) -> bool {
    let mailbox = discord::mailbox_snapshot(shared, channel).await;
    release_stale_mailbox_ownership_after_force_clean(
        shared,
        &PROVIDER,
        channel,
        mailbox.active_user_message_id.map(MessageId::get),
        mailbox.active_turn_nonce,
        Instant::now(),
    )
    .await
}

/// One retry-queue respawn: pin whatever row is on disk, as the queue does.
async fn respawn_attempt(
    registry: &HealthRegistry,
    channel: ChannelId,
    tmux: &str,
) -> Result<(), RebindError> {
    let pin = inflight::load_inflight_state(&PROVIDER, channel.get())
        .as_ref()
        .map(InflightEpisodePin::from_state);
    registry
        .rebind_inflight_after_force_clean(
            &PROVIDER,
            channel.get(),
            Some(tmux.to_string()),
            None,
            pin.as_ref(),
        )
        .await
        .expect("fixture: the registered runtime must resolve")
        .map(|_| ())
}

fn is_already_exists(result: &Result<(), RebindError>) -> bool {
    matches!(result, Err(RebindError::InflightAlreadyExists))
}

/// Persist `row` as it looks after the given idle time with nothing advancing it.
fn write_aged_row(fixture: &Fixture, row: &InflightTurnState, idle_secs: i64) {
    let mut aged = row.clone();
    let stamp = (chrono::Local::now() - chrono::Duration::seconds(idle_secs))
        .format("%Y-%m-%d %H:%M:%S")
        .to_string();
    aged.started_at = stamp.clone();
    aged.updated_at = stamp;
    let path = fixture.row_path(ChannelId::new(row.channel_id));
    std::fs::create_dir_all(path.parent().expect("inflight dir")).expect("create inflight dir");
    std::fs::write(
        &path,
        serde_json::to_string_pretty(&aged).expect("serialize row"),
    )
    .expect("write aged row");
}

/// Live tmux, no watcher, a watcherless TUI-direct row: every retry of the
/// force-clean respawn is refused the same way, so the channel never recovers.
#[test]
fn force_clean_respawn_makes_progress_against_a_watcherless_tui_direct_row() {
    let fixture = Fixture::new(true);
    block_on(async {
        let shared = discord::make_shared_data_for_tests();
        let registry = registry_with(&shared).await;
        let channel = ChannelId::new(6_159_100_001);
        let tmux = "AgentDesk-claude-i6159-deadlock";

        claim_row(&watcherless_tui_direct_row(channel, tmux));
        engage_mailbox(&shared, channel).await;
        assert!(!shared.tmux_watchers.contains_key(&channel));

        let snapshot = registry
            .snapshot_watcher_state_for_shared(&PROVIDER, shared.clone(), channel.get())
            .await
            .expect("fixture: snapshot resolves");
        assert!(
            force_clean_should_respawn_watcher(&snapshot),
            "fixture: live AgentDesk tmux must qualify for a respawn (snapshot tmux={:?} alive={:?})",
            snapshot.tmux_session,
            snapshot.tmux_session_alive,
        );
        assert!(
            force_clean_release(&shared, channel).await,
            "fixture: force-clean must release the stale mailbox turn"
        );

        let row_before = std::fs::read(fixture.row_path(channel)).expect("row on disk");
        let mut attempts = Vec::new();
        for _ in 0..super::WATCHER_RESPAWN_MAX_ATTEMPTS {
            let result = respawn_attempt(&registry, channel, tmux).await;
            let refused = is_already_exists(&result);
            attempts.push(result);
            if !refused {
                break;
            }
        }
        let row_after = std::fs::read(fixture.row_path(channel)).ok();

        assert!(
            !attempts.iter().all(is_already_exists),
            "all {} force-clean respawn attempts failed with InflightAlreadyExists: {:?}; \
             row unchanged across attempts: {}; watcher still absent: {}",
            attempts.len(),
            attempts,
            row_after.as_deref() == Some(row_before.as_slice()),
            !shared.tmux_watchers.contains_key(&channel),
        );
    });
}

/// Whatever lone object makes the rebind guard refuse must be something the
/// force-clean cleaner removes. Both sets are measured, not hard-coded. The
/// guard runs before the tmux probe, so a dead tmux stops a passing rebind
/// at `TmuxNotAlive` instead of spawning a watcher.
#[test]
fn force_clean_removes_everything_the_rebind_guard_refuses_on() {
    const ROW: &str = "inflight_row";
    const TOKEN: &str = "mailbox_cancel_token";
    let tmux = "AgentDesk-claude-i6159-mismatch";

    let fixture = Fixture::new(false);
    let (guard_checked, row_only, token_only) = block_on(async {
        let mut guard_checked = BTreeSet::new();

        let shared = discord::make_shared_data_for_tests();
        let registry = registry_with(&shared).await;
        let row_channel = ChannelId::new(6_159_200_001);
        claim_row(&watcherless_tui_direct_row(row_channel, tmux));
        let row_only = respawn_attempt(&registry, row_channel, tmux).await;
        if is_already_exists(&row_only) {
            guard_checked.insert(ROW);
        }

        let token_channel = ChannelId::new(6_159_200_002);
        engage_mailbox(&shared, token_channel).await;
        let token_only = respawn_attempt(&registry, token_channel, tmux).await;
        if is_already_exists(&token_only) {
            guard_checked.insert(TOKEN);
        }
        (guard_checked, row_only, token_only)
    });

    let cleaner_removed = block_on(async {
        let mut removed = BTreeSet::new();
        let shared = discord::make_shared_data_for_tests();
        let channel = ChannelId::new(6_159_200_003);
        claim_row(&watcherless_tui_direct_row(channel, tmux));
        engage_mailbox(&shared, channel).await;

        force_clean_release(&shared, channel).await;
        if !fixture.row_path(channel).exists() {
            removed.insert(ROW);
        }
        if discord::mailbox_snapshot(&shared, channel)
            .await
            .cancel_token
            .is_none()
        {
            removed.insert(TOKEN);
        }
        removed
    });
    assert!(
        !cleaner_removed.is_empty(),
        "fixture: the cleaner must remove at least the stale mailbox turn"
    );

    let unreleased: Vec<_> = guard_checked.difference(&cleaner_removed).collect();
    assert!(
        unreleased.is_empty(),
        "rebind guard refuses on {guard_checked:?} but force-clean removes only \
         {cleaner_removed:?}; left blocking: {unreleased:?} \
         (row-only rebind: {row_only:?}, token-only rebind: {token_only:?})"
    );
}

/// With no watcher anywhere, the row writers keep refreshing the row in place,
/// while the respawn that would give it a watcher is refused because it exists.
#[test]
fn a_watcherless_row_is_not_kept_alive_while_respawn_is_refused() {
    let fixture = Fixture::new(true);
    block_on(async {
        let shared = discord::make_shared_data_for_tests();
        let registry = registry_with(&shared).await;
        let channel = ChannelId::new(6_159_300_001);
        let tmux = "AgentDesk-claude-i6159-decoupled";
        assert!(!shared.tmux_watchers.contains_key(&channel));

        write_aged_row(
            &fixture,
            &watcherless_tui_direct_row(channel, tmux),
            100_000,
        );
        let aged = inflight::load_inflight_state(&PROVIDER, channel.get()).expect("aged row loads");
        let identity = InflightTurnIdentity::from_state(&aged);

        let downgrade = inflight::downgrade_orphaned_session_bound_relay_owner_locked(
            &PROVIDER,
            channel.get(),
            &identity,
            tmux,
        );
        assert_eq!(
            downgrade,
            OrphanRelayReclaimOutcome::Downgraded,
            "fixture: the aged watcherless row must match the orphan shape"
        );
        let downgraded = inflight::load_inflight_state(&PROVIDER, channel.get());
        let heartbeat = inflight::touch_inflight_state_if_matches_identity(
            &PROVIDER,
            channel.get(),
            &identity,
            "respawn_deadlock_tests",
        );
        assert_ne!(
            heartbeat,
            GuardedSaveOutcome::IoError,
            "fixture: heartbeat I/O"
        );
        let refreshed = inflight::load_inflight_state(&PROVIDER, channel.get());

        let row_kept_alive = refreshed
            .as_ref()
            .is_some_and(|row| row.updated_at != aged.updated_at);
        let respawn = respawn_attempt(&registry, channel, tmux).await;

        assert!(
            !(row_kept_alive && is_already_exists(&respawn)),
            "no watcher exists, yet the row survived and was refreshed \
             (downgrade: {downgrade:?} -> owner {:?}; heartbeat: {heartbeat:?}; \
             updated_at {} -> {:?}) and the respawn was refused: {respawn:?}",
            downgraded
                .as_ref()
                .map(InflightTurnState::effective_relay_owner_kind),
            aged.updated_at,
            refreshed.as_ref().map(|row| row.updated_at.as_str()),
        );
    });
}
