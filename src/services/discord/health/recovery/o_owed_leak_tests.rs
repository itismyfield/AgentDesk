//! A stale leak on a channel O owns settles its row at the leaked tail only when O's init covers
//! the turn's transcript from the turn's start; otherwise the row stays as it was.

use super::super::HealthRegistry;
use super::maybe_recover_completed_stale_leak;
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::discord::inflight::{self, InflightTurnState};
use crate::services::discord::turn_finalizer::tests::with_isolated_runtime_root;
use crate::services::provider::ProviderKind;
use crate::services::tui_o::cutover::test_override;
use crate::services::tui_o::store::{InitSource, Initialized, OStore, StoreConfig};
use poise::serenity_prelude::ChannelId;
use std::path::{Path, PathBuf};

/// A watcher-owned turn from `start` of `path` whose answer never reached its placeholder.
fn leaked_row(channel: u64, path: &Path, start: u64) -> InflightTurnState {
    let mut state: InflightTurnState = serde_json::from_value(serde_json::json!({
        "version": 9, "provider": "claude", "channel_id": channel, "channel_name": "adk-cc",
        "request_owner_user_id": 7, "user_msg_id": channel + 1, "current_msg_id": channel + 2,
        "current_msg_len": 0, "user_text": "prompt", "source": "text", "session_id": "session",
        "tmux_session_name": "AgentDesk-claude-adk-cc", "output_path": path,
        "input_fifo_path": null, "last_offset": start, "full_response": "leaked answer body",
        "response_sent_offset": 0, "relay_owner_kind": "watcher", "runtime_kind": "claude_tui",
        "started_at": "2026-01-01 00:00:00", "updated_at": "2026-01-01 00:00:00"
    }))
    .unwrap();
    state.turn_start_offset = Some(start);
    inflight::save_inflight_state(&state).unwrap();
    state
}

/// One era whose init attaches each channel's transcript from its own delivery start.
fn init_over(sources: &[(u64, PathBuf, u64)]) {
    let root = crate::config::runtime_root().unwrap();
    let config = StoreConfig { enabled: true };
    let store = OStore::open_if_enabled(&config, &root).unwrap().unwrap();
    let source_id = crate::services::tui_o::shadow::binding_reader::source_id_for;
    let at = chrono::Utc::now();
    let channels: Vec<u64> = sources.iter().map(|(channel, _, _)| *channel).collect();
    let init = |channel| {
        let (_, path, delivery_start) = sources.iter().find(|(c, _, _)| *c == channel).unwrap();
        let source_id = source_id("session", path).unwrap();
        let prefix_hash = String::new();
        let sources = vec![InitSource {
            source_id,
            delivery_start: *delivery_start,
            prefix_hash,
        }];
        let (initial_anchor, build_digest) = (0, "test".into());
        Ok(Initialized {
            policy_parent: None,
            channel,
            sources,
            initial_anchor,
            build_digest,
            at,
        })
    };
    store.begin_fixture_era(&channels, at, init).unwrap();
}

/// O owns both channels; only the one whose init starts at or before the turn settles its row.
#[tokio::test(flavor = "current_thread")]
async fn a_stale_leak_o_owes_settles_its_row_and_one_o_does_not_owe_stays() {
    with_isolated_runtime_root(|| async move {
        let transcripts = tempfile::tempdir().unwrap();
        let (owed, late, start) = (9_433_061u64, 9_433_062u64, 16);
        let cases = [(owed, start, true), (late, start + 1, false)];
        let mut sources = Vec::new();
        for (channel, delivery_start, _) in cases {
            let path = transcripts.path().join(format!("{channel}.jsonl"));
            std::fs::write(&path, "x".repeat(64)).unwrap();
            sources.push((channel, path, delivery_start));
        }
        init_over(&sources);
        let _owned = test_override::force_channels(&[(owed, ClaudeTui), (late, ClaudeTui)]);
        let shared = crate::services::discord::make_shared_data_for_tests();
        let registry = HealthRegistry::new();
        registry.register("claude".into(), shared.clone()).await;
        for ((channel, _, settles), (_, path, _)) in cases.into_iter().zip(&sources) {
            let state = leaked_row(channel, path, start);
            let recover = maybe_recover_completed_stale_leak;
            let provider = ProviderKind::Claude;
            let recovered = recover(&registry, &provider, &shared, ChannelId::new(channel));
            assert_eq!(recovered.await, settles, "{channel}");
            let row = inflight::load_inflight_state(&provider, channel).unwrap();
            let end = if settles {
                state.full_response.len()
            } else {
                0
            };
            assert_eq!(row.response_sent_offset, end, "{channel}");
        }
    })
    .await;
}
