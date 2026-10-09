//! The absence retry under the input fence: a closed gate refuses before the retry's first read
//! and keeps the entry's budget; an admitted retry holds the drain until it returns.

use super::super::claude_original_tests::respawn_gap;
use super::*;
use crate::services::discord::input_runtime::{self, fence::Gate};
use futures::FutureExt;

#[tokio::test]
async fn c2b_closed_gate_refuses_the_retry_before_any_read_and_keeps_its_budget() {
    let _absence = lock_watcher_absence_for_test().await;
    let provider = ProviderKind::Codex;
    let channel = ChannelId::new(6_325_955);
    seed_live_bridge_respawn_test(channel);
    let key = WatcherAbsenceKey::new(&provider, channel);
    {
        let mut state = WATCHER_ABSENCE.get_mut(&key).unwrap();
        state.failed_attempts = 2;
        state.next_attempt_unix_secs = 500;
    }
    let gate = Gate::protect(provider.clone(), channel.get()).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);
    let _closing = gate.close().unwrap();
    let registry = HealthRegistry::new();
    let runtimes = [discord::make_shared_data_for_tests()];

    let attempted =
        retry_pending_watcher_respawn(&registry, &provider, &runtimes, channel, 1_000).await;

    assert!(!attempted);
    assert_eq!(
        live_bridge_respawn_test_counts(channel),
        [0, 0, 2],
        "no snapshot, no reclaim, no charged attempt"
    );
    assert_eq!(
        WATCHER_ABSENCE.get(&key).unwrap().next_attempt_unix_secs,
        500
    );
    assert!(
        input_runtime::health_reasons()
            .iter()
            .any(|reason| reason.contains(&format!("channel={}", channel.get()))),
        "the refusal is a health reason"
    );
    clear_watcher_absence(&provider, channel);
}

#[tokio::test]
async fn c2b_admitted_retry_holds_the_input_drain_until_it_returns() {
    let _absence = lock_watcher_absence_for_test().await;
    let provider = ProviderKind::Codex;
    let channel = ChannelId::new(6_325_956);
    seed_live_bridge_respawn_test(channel);
    let gate = Gate::protect(provider.clone(), channel.get()).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);
    // An empty registry makes the respawn fail fast once the retry resumes.
    let registry = HealthRegistry::new();
    let runtimes = [discord::make_shared_data_for_tests()];
    let (reached, resume) = respawn_gap::arm(channel.get());

    let retry = retry_pending_watcher_respawn(&registry, &provider, &runtimes, channel, 1_000);
    tokio::pin!(retry);
    tokio::select! {
        _ = &mut retry => panic!("the retry must pause inside its admitted body"),
        _ = reached.notified() => {}
    }
    let closing = gate.close().unwrap();
    assert!(
        closing.drain().now_or_never().is_none(),
        "the admitted retry holds the drain"
    );
    resume.notify_one();
    assert!(retry.await, "the admitted retry attempted its respawn");
    assert!(
        closing.drain().now_or_never().is_some(),
        "returning releases the drain"
    );
    clear_watcher_absence(&provider, channel);
}

/// A `tmux` that reports every pane alive and lists no sessions.
const FAKE_TMUX: &str = "#!/bin/sh\nwhile [ \"${1#-}\" != \"$1\" ]; do shift; done\ncase \"$1\" in list-panes) echo 0 ;; esac; exit 0\n";

/// An admitted retry on a protected open channel backfills and rebinds an old-format orphan row;
/// a gate closed mid-retry waits for it, and its return releases the drain.
#[test]
fn c2b_admitted_retry_rebinds_an_old_format_row_through_a_closing_gate() {
    use crate::services::agent_protocol::RuntimeHandoffKind;
    use crate::services::session_host::test_support::InjectedLivenessGuard;
    use crate::services::session_host::{HostLiveness, HostSessionRef};
    use std::os::unix::fs::PermissionsExt;
    let _absence = blocking_lock_watcher_absence_for_test();
    let root = tempfile::tempdir().unwrap();
    let _root_env = crate::config::TestEnvVarGuard::set_path("AGENTDESK_ROOT_DIR", root.path());
    let tmux_dir = tempfile::tempdir().unwrap();
    let script = tmux_dir.path().join("tmux");
    std::fs::write(&script, FAKE_TMUX).unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
    let _path =
        crate::config::TestEnvVarGuard::prepend_path_after_shared_test_env_lock(tmux_dir.path());
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(6_325_957);
    let tmux = format!(
        "AgentDesk-claude-c2rt{}-{}-cc",
        channel.get(),
        std::process::id()
    );
    crate::services::tmux_common::write_tmux_runtime_kind_marker(
        &tmux,
        RuntimeHandoffKind::ClaudeTui,
    )
    .unwrap();
    let session = format!("48fdb7f3-6325-4000-8000-{:012}", channel.get());
    let transcript = root.path().join(format!("{session}.jsonl"));
    std::fs::write(&transcript, vec![b'x'; 4_096]).unwrap();
    let mut orphan = discord::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        0,
        0,
        channel.get() + 7,
        String::new(),
        Some(session),
        Some(tmux.clone()),
        Some(transcript.display().to_string()),
        None,
        4_096,
    );
    orphan.turn_source = discord::inflight::TurnSource::ExternalInput;
    orphan.set_relay_owner_kind(discord::inflight::RelayOwnerKind::Watcher);
    assert!(discord::inflight::save_inflight_state_if_absent(&orphan).unwrap());
    let row = input_runtime::fence::population_root()
        .unwrap()
        .join("discord_inflight/claude")
        .join(format!("{}.json", channel.get()));
    let mut old: serde_json::Value = serde_json::from_slice(&std::fs::read(&row).unwrap()).unwrap();
    old.as_object_mut().unwrap().remove("finalizer_turn_id");
    std::fs::write(&row, serde_json::to_vec_pretty(&old).unwrap()).unwrap();
    let before = std::fs::read(&row).unwrap();
    let _live = InjectedLivenessGuard::set(HostSessionRef::tmux(&tmux), HostLiveness::Live);
    let key = WatcherAbsenceKey::new(&provider, channel);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let shared = discord::make_shared_data_for_tests();
        let registry = HealthRegistry::new();
        registry
            .register(provider.as_str().to_string(), shared.clone())
            .await;
        registry
            .register_http(
                provider.as_str().to_string(),
                Arc::new(poise::serenity_prelude::Http::new("Bot test-token")),
            )
            .await;
        WATCHER_ABSENCE.insert(key.clone(), WatcherAbsenceState::newly_absent(0));
        let gate = Gate::protect(provider.clone(), channel.get()).unwrap();
        let _health = input_runtime::fence::test_health::Clear::new(&gate);
        let (reached, resume) = respawn_gap::arm(channel.get());

        let runtimes = [shared.clone()];
        let retry = retry_pending_watcher_respawn(&registry, &provider, &runtimes, channel, 1_000);
        tokio::pin!(retry);
        tokio::select! {
            _ = &mut retry => panic!("the retry must pause inside its admitted body"),
            _ = reached.notified() => {}
        }
        let closing = gate.close().unwrap();
        assert!(
            closing.drain().now_or_never().is_none(),
            "the admitted retry holds the drain"
        );
        resume.notify_one();
        assert!(retry.await, "the admitted retry attempted its respawn");

        let watcher = shared
            .tmux_watchers
            .remove(&channel)
            .map(|(_, watcher)| watcher);
        if let Some(watcher) = watcher.as_ref() {
            watcher.cancel.store(true, Ordering::Relaxed);
        }
        assert!(watcher.is_some(), "the retry rebound a watcher");
        assert!(
            WATCHER_ABSENCE.get(&key).is_none(),
            "the respawned watcher cleared the absence"
        );
        let after = std::fs::read(&row).unwrap();
        assert_ne!(after, before, "the retry rewrote its row");
        let after: serde_json::Value = serde_json::from_slice(&after).unwrap();
        assert!(
            after.get("finalizer_turn_id").is_some(),
            "the old-format row was backfilled"
        );
        assert!(
            !input_runtime::health_reasons()
                .iter()
                .any(|reason| reason.contains(&format!("channel={}", channel.get()))),
            "the admitted retry saw a refused writer"
        );
        assert!(
            closing.drain().now_or_never().is_some(),
            "returning releases the drain"
        );
    });
    drop(runtime);
    clear_watcher_absence(&provider, channel);
}

/// An admitted reclaim of a watcherless session-bound orphan on a protected open channel
/// downgrades the row where its writer is allowed, instead of being refused.
#[tokio::test]
async fn c2b_admitted_reclaim_downgrades_a_session_bound_orphan_without_a_refusal() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let provider = ProviderKind::Claude;
    let channel = ChannelId::new(6_325_959);
    let transcript = root.path().join("session-bound.jsonl");
    std::fs::write(&transcript, vec![b'x'; 512]).unwrap();
    let mut orphan = discord::inflight::InflightTurnState::new(
        provider.clone(),
        channel.get(),
        None,
        7,
        6_325_969,
        0,
        "typed in TUI".to_string(),
        None,
        Some(format!(
            "AgentDesk-claude-c2rc{}-{}",
            channel.get(),
            std::process::id()
        )),
        Some(transcript.display().to_string()),
        None,
        0,
    );
    orphan.turn_source = discord::inflight::TurnSource::ExternalInput;
    orphan.set_relay_owner_kind(discord::inflight::RelayOwnerKind::SessionBoundRelay);
    orphan.injected_prompt_message_id = Some(6_325_979);
    assert!(discord::inflight::save_inflight_state_if_absent(&orphan).unwrap());
    let pin = discord::inflight::InflightEpisodePin::from_state(
        &discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap(),
    );
    let shared = discord::make_shared_data_for_tests();
    let registry = HealthRegistry::new();
    registry
        .register(provider.as_str().to_string(), shared.clone())
        .await;
    let gate = Gate::protect(provider.clone(), channel.get()).unwrap();
    let _health = input_runtime::fence::test_health::Clear::new(&gate);
    let permit = gate.admit().unwrap();

    input_runtime::fence::effect::scope(
        Some(permit),
        reclaim_watcherless_session_bound_relay(&registry, &provider, channel, &pin),
    )
    .await;

    let after = discord::inflight::load_inflight_state_read_only(&provider, channel.get()).unwrap();
    assert_eq!(
        after.effective_relay_owner_kind(),
        discord::inflight::RelayOwnerKind::None,
        "the orphan was downgraded to ownerless recovery"
    );
    assert!(
        !input_runtime::health_reasons()
            .iter()
            .any(|reason| reason.contains(&format!("channel={}", channel.get()))),
        "the admitted reclaim saw a refused writer"
    );
}
