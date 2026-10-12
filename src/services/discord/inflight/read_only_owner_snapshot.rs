//! Read-only episode evidence uses the existing mailbox incarnation and never repairs its row.

use super::load_inflight_state_read_only_result;
use crate::services::provider::{CancelToken, ProviderKind, cancel_requested};
use crate::services::tui_prompt_dedupe::codex_verified::provenance::TurnEpisodeRef;
use crate::services::turn_orchestrator::{ChannelMailboxHandle, ChannelMailboxRegistry};
use poise::serenity_prelude::ChannelId;
use std::sync::Arc;

pub(crate) struct CodexEpisodeOwnerSnapshot {
    mailbox: ChannelMailboxHandle,
    token: Arc<CancelToken>,
    episode: TurnEpisodeRef,
    tmux: String,
}

pub(crate) async fn codex_episode_snapshot(
    channel: u64,
    tmux: &str,
    token: &Arc<CancelToken>,
) -> Result<CodexEpisodeOwnerSnapshot, &'static str> {
    if channel == 0 || tmux.is_empty() {
        return Err("episode_identity_missing");
    }
    let mailbox = ChannelMailboxRegistry::global_handle(ChannelId::new(channel))
        .ok_or("episode_mailbox_missing")?;
    let mut snapshot = CodexEpisodeOwnerSnapshot {
        mailbox,
        token: Arc::clone(token),
        episode: TurnEpisodeRef {
            channel_id: channel,
            user_message_id: 0,
            request_owner_id: 0,
            turn_nonce: String::new(),
            native_turn_id: String::new(),
        },
        tmux: tmux.to_owned(),
    };
    snapshot.episode = snapshot.observe().await?;
    Ok(snapshot)
}

impl CodexEpisodeOwnerSnapshot {
    pub(crate) fn episode(&self) -> &TurnEpisodeRef {
        &self.episode
    }

    pub(crate) fn token(&self) -> &Arc<CancelToken> {
        &self.token
    }

    pub(crate) async fn recheck(&self) -> Result<(), &'static str> {
        (self.observe().await? == self.episode)
            .then_some(())
            .ok_or("episode_row_changed")
    }

    async fn observe(&self) -> Result<TurnEpisodeRef, &'static str> {
        let channel = self.episode.channel_id;
        let current = ChannelMailboxRegistry::global_handle(ChannelId::new(channel))
            .ok_or("episode_mailbox_missing")?;
        if !current.same_actor(&self.mailbox) {
            return Err("episode_mailbox_changed");
        }
        let active = self
            .mailbox
            .try_snapshot()
            .await
            .map_err(|_| "episode_mailbox_unreachable")?;
        let row = load_inflight_state_read_only_result(&ProviderKind::Codex, channel)
            .map_err(|_| "episode_row_unreadable")?
            .ok_or("episode_row_missing")?;
        let nonce = self.token.turn_nonce().ok_or("episode_nonce_missing")?;
        if cancel_requested(Some(self.token.as_ref()))
            || !active
                .cancel_token
                .as_ref()
                .is_some_and(|active| Arc::ptr_eq(active, &self.token))
            || row.provider != "codex"
            || row.channel_id != channel
            || row.tmux_session_name.as_deref() != Some(&self.tmux)
            || row.user_msg_id == 0
            || row.request_owner_user_id == 0
            || row.turn_nonce.as_deref() != Some(nonce)
            || active.active_turn_nonce.as_deref() != Some(nonce)
            || active.active_request_owner.map(|id| id.get()) != Some(row.request_owner_user_id)
            || active.active_user_message_id.map(|id| id.get()) != Some(row.user_msg_id)
        {
            return Err("episode_owner_mismatch");
        }
        Ok(TurnEpisodeRef {
            channel_id: channel,
            user_message_id: row.user_msg_id,
            request_owner_id: row.request_owner_user_id,
            turn_nonce: nonce.to_owned(),
            native_turn_id: String::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        InflightTurnState, inflight_runtime_root, inflight_state_path, save_inflight_state_in_root,
    };
    use crate::services::discord::inflight::o_seed_observation::Guard;
    use crate::services::discord::{CodexEpisodeOwnerSnapshot, codex_episode_snapshot};
    use crate::services::provider::{CancelToken, ProviderKind};
    use crate::services::turn_orchestrator::ChannelMailboxRegistry;
    use poise::serenity_prelude::{ChannelId, MessageId, UserId};
    use std::sync::{Arc, atomic::Ordering};

    const CHANNEL: u64 = 5_845_201;
    const TMUX: &str = "s2b1a-row-snapshot";

    struct Fixture {
        runtime: tokio::runtime::Runtime,
        registry: ChannelMailboxRegistry,
        token: Arc<CancelToken>,
        path: std::path::PathBuf,
        _env: crate::config::TestEnvVarGuard,
        _root: tempfile::TempDir,
        _lock: crate::config::test_env_lock::SharedTestEnvLockGuard,
    }

    impl Fixture {
        fn new() -> Self {
            let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
            let root = tempfile::tempdir().unwrap();
            let env = crate::config::TestEnvVarGuard::set_path_after_shared_test_env_lock(
                "AGENTDESK_ROOT_DIR",
                root.path(),
            );
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();
            let registry = ChannelMailboxRegistry::default();
            let token = Arc::new(CancelToken::new());
            runtime.block_on(async {
                assert!(
                    registry
                        .handle(ChannelId::new(CHANNEL))
                        .try_start_turn(Arc::clone(&token), UserId::new(8), MessageId::new(30))
                        .await
                );
            });
            let mut row = InflightTurnState::new(
                ProviderKind::Codex,
                CHANNEL,
                None,
                8,
                30,
                31,
                "prompt".into(),
                None,
                Some(TMUX.into()),
                None,
                None,
                0,
            );
            row.turn_nonce = token.turn_nonce().map(str::to_owned);
            let dir = inflight_runtime_root().unwrap();
            save_inflight_state_in_root(&dir, &row).unwrap();
            let path = inflight_state_path(&dir, &ProviderKind::Codex, CHANNEL);
            Self {
                runtime,
                registry,
                token,
                path,
                _env: env,
                _root: root,
                _lock: lock,
            }
        }

        fn snapshot(
            &self,
            token: &Arc<CancelToken>,
        ) -> Result<CodexEpisodeOwnerSnapshot, &'static str> {
            self.runtime
                .block_on(codex_episode_snapshot(CHANNEL, TMUX, token))
        }
    }

    #[test]
    fn s2b1a_row_snapshot_reads_legacy_bytes_without_backfill_or_generation_change() {
        let fixture = Fixture::new();
        let mut row: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&fixture.path).unwrap()).unwrap();
        row.as_object_mut().unwrap().remove("finalizer_turn_id");
        std::fs::write(&fixture.path, serde_json::to_vec(&row).unwrap()).unwrap();
        let before = std::fs::read(&fixture.path).unwrap();
        let observed = Guard::new(&ProviderKind::Codex, CHANNEL);
        let snapshot = fixture.snapshot(&fixture.token).unwrap();
        assert!(Arc::ptr_eq(snapshot.token(), &fixture.token));
        assert_eq!(snapshot.episode().user_message_id, 30);
        assert_eq!(snapshot.episode().request_owner_id, 8);
        assert_eq!(
            snapshot.episode().turn_nonce,
            fixture.token.turn_nonce().unwrap()
        );
        fixture.runtime.block_on(snapshot.recheck()).unwrap();
        assert_eq!(std::fs::read(&fixture.path).unwrap(), before);
        let after: serde_json::Value = serde_json::from_slice(&before).unwrap();
        assert_eq!(after["save_generation"], row["save_generation"]);
        let counts = observed.snapshot();
        assert_eq!(counts.readonly_inflight_load_calls, 2);
        assert_eq!(counts.writable_inflight_load_calls, 0);
        assert_eq!(counts.compatibility_backfill_attempts, 0);
    }

    #[test]
    fn s2b1a_row_snapshot_rejects_same_nonce_different_arc_and_successor_mailbox() {
        let fixture = Fixture::new();
        let duplicate = Arc::new(CancelToken::from_persisted_turn_nonce(
            fixture.token.turn_nonce().map(str::to_owned),
        ));
        assert!(
            fixture.snapshot(&duplicate).is_err(),
            "a restored token cannot replace the active Arc"
        );
        let snapshot = fixture.snapshot(&fixture.token).unwrap();
        let successor = ChannelMailboxRegistry::default();
        fixture.runtime.block_on(async {
            assert!(
                successor
                    .handle(ChannelId::new(CHANNEL))
                    .try_start_turn(
                        Arc::clone(&fixture.token),
                        UserId::new(8),
                        MessageId::new(30)
                    )
                    .await
            );
            assert!(
                snapshot.recheck().await.is_err(),
                "the captured handle cannot follow a new actor"
            );
        });
        assert!(
            !fixture
                .registry
                .handle(ChannelId::new(CHANNEL))
                .same_actor(&successor.handle(ChannelId::new(CHANNEL)))
        );
    }

    #[test]
    fn s2b1a_row_snapshot_refuses_unknown_owner_nonce_and_unreadable_rows() {
        let fixture = Fixture::new();
        let original = std::fs::read(&fixture.path).unwrap();
        for (field, value) in [
            ("request_owner_user_id", serde_json::json!(9)),
            ("user_msg_id", serde_json::json!(0)),
            ("turn_nonce", serde_json::Value::Null),
            ("provider", serde_json::json!("claude")),
            ("channel_id", serde_json::json!(CHANNEL + 1)),
            ("tmux_session_name", serde_json::json!("foreign")),
        ] {
            let mut row: serde_json::Value = serde_json::from_slice(&original).unwrap();
            row[field] = value;
            let bytes = serde_json::to_vec(&row).unwrap();
            std::fs::write(&fixture.path, &bytes).unwrap();
            assert!(
                fixture.snapshot(&fixture.token).is_err(),
                "accepted {field}"
            );
            assert_eq!(std::fs::read(&fixture.path).unwrap(), bytes);
        }
        std::fs::write(&fixture.path, b"{torn").unwrap();
        assert!(fixture.snapshot(&fixture.token).is_err());
        std::fs::write(&fixture.path, original).unwrap();
        fixture.token.cancelled.store(true, Ordering::Release);
        assert!(fixture.snapshot(&fixture.token).is_err());
    }
}

#[cfg(test)]
pub(crate) fn save_actual_episode_for_tests(
    channel: u64,
    tmux: &str,
    message: u64,
    token: &Arc<CancelToken>,
) -> std::path::PathBuf {
    crate::config::test_env_lock::assert_shared_test_env_lock_held();
    let mut row = super::InflightTurnState::new(
        ProviderKind::Codex,
        channel,
        None,
        8,
        message,
        message + 1,
        "actual warm prompt".into(),
        None,
        Some(tmux.into()),
        None,
        None,
        0,
    );
    row.turn_nonce = token.turn_nonce().map(str::to_owned);
    let root = super::inflight_runtime_root().unwrap();
    super::save_inflight_state_in_root(&root, &row).unwrap();
    super::inflight_state_path(&root, &ProviderKind::Codex, channel)
}
