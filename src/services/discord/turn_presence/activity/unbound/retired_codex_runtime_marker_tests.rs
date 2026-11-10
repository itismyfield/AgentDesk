//! A retired Codex row's old tmux runtime marker must not hold its next queued input forever.

use super::super::*;
use crate::db::dispatched_sessions::hosted_execution::{
    HostedExecution, HostedLocation, HostedOwner, HostedRecord, HostedState, tests::record,
};
use crate::services::discord::mailbox_snapshot;
use crate::services::discord::recovery_engine::herdr_reader::{
    PASSES, reconnect_restarted_herdr_with_shared,
};
use crate::services::discord::turn_presence::activity::{activity_now, reading_now};
use crate::services::session_host::herdr_socket_rig_tests::HerdrRig;
use crate::services::turn_orchestrator::{Intervention, InterventionMode};
use poise::serenity_prelude::{MessageId, UserId};

struct RetiredCodex {
    rig: Rig,
    key: String,
    execution: HostedExecution,
}

impl RetiredCodex {
    async fn new(id: u64) -> Self {
        let live = crate::config_live_reload::test_live_guard(
            crate::config::test_env_lock::acquire_shared_test_env_lock(),
        );
        let mut rig = Rig::with_live_guard(id, true, false, live).await;
        let agent = &mut rig.config.agents[0];
        agent.provider = "codex".into();
        let channel = agent.channels.get("claude").unwrap().clone();
        agent.channels = crate::config::AgentChannels::new().with("codex", channel);
        crate::config_live_reload::install(rig.config.clone());
        std::fs::write(
            crate::runtime_layout::config_file_path(rig.root.path()),
            serde_yaml::to_string(&rig.config).unwrap(),
        )
        .unwrap();
        rig.shared
            .restart
            .reconcile_done
            .store(true, Ordering::Release);
        let owner = HostedOwner {
            provider: "codex".into(),
            discord_token_hash: rig.shared.token_hash.clone(),
            channel_id: id.to_string(),
            logical_key: ProviderKind::Codex.build_tmux_session_name(&format!("s1e-{id}")),
            owner_node: "test-node".into(),
            runtime_root: rig.root.path().display().to_string(),
        };
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let mut execution = record(&owner, &nonce, HostedState::Retired);
        execution.expected.as_mut().unwrap().binding_provider = "codex".into();
        let endpoint = crate::config::session_hosts::herdr_endpoint(id).unwrap();
        execution.location = Some(HostedLocation {
            host: "herdr".into(),
            execution_node: endpoint.execution_node,
            endpoint_config_key: endpoint.key,
            socket_addr: endpoint.socket_path.display().to_string(),
            named_session: endpoint.herdr_session,
            pane_id: "retired-codex-pane".into(),
        });
        let key = adk_session::build_namespaced_session_key(
            &rig.shared.token_hash,
            &ProviderKind::Codex,
            &owner.logical_key,
        );
        sqlx::query(
            "INSERT INTO sessions (session_key, provider, status, identity_kind,
                                   discord_token_hash, channel_id, hosted_execution)
             VALUES ($1, 'codex', 'idle', 'discord_channel', $2, $3, $4)",
        )
        .bind(&key)
        .bind(&owner.discord_token_hash)
        .bind(&owner.channel_id)
        .bind(serde_json::to_value(&execution).unwrap())
        .execute(rig.shared.pg_pool.as_ref().unwrap())
        .await
        .unwrap();
        for (extension, value) in [
            ("host_kind", "herdr"),
            ("spawn_nonce", nonce.as_str()),
            (tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT, "codex_tui"),
        ] {
            let path = marker_path(&owner.logical_key, extension);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, value).unwrap();
        }
        Self {
            rig,
            key,
            execution,
        }
    }

    async fn current(&self) -> HostedRecord {
        match load_hosted_execution_pg(
            self.rig.shared.pg_pool.as_ref().unwrap(),
            HostedLookupKey::SessionKey(&self.key),
        )
        .await
        {
            HostedLookup::Found(row) => row.record,
            other => panic!("canonical retired row disappeared: {other:?}"),
        }
    }
}

fn queued_input() -> Intervention {
    let message_id = MessageId::new(8_069_201);
    Intervention {
        author_id: UserId::new(8_069_202),
        author_is_bot: false,
        message_id,
        queued_generation: 1,
        source_message_ids: vec![message_id],
        source_message_queued_generations: Vec::new(),
        source_text_segments: Vec::new(),
        text: "retired Codex 복구 뒤의 새 입력".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
    }
}

#[test]
fn i8069_restart_reader_clears_retired_codex_presence_and_releases_queued_input_pg() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    let fixture = runtime.block_on(RetiredCodex::new(69_186_901));
    let rig = &fixture.rig;
    let channel = rig.channel;
    let provider = ProviderKind::Codex;
    let _turn = crate::services::tui_o::turn_mode::TestConfirmation::new(channel.get());
    // Both presence and cleanup use the default socket under the fixture's isolated TMUX_TMPDIR.
    let _pane = crate::config::TestEnvVarGuard::remove_after_shared_test_env_lock("TMUX");
    let trace = ChannelProbeTrace::new(channel.get());
    let _switch = crate::services::turn_host::force_codex_switch_for_test(Some(true));
    let socket = HerdrRig::start();
    let endpoint = crate::config::session_hosts::herdr_endpoint(channel.get()).unwrap();
    let _registry = socket.registry_as_on_this_thread(
        &endpoint.execution_node,
        &endpoint.key,
        &endpoint.socket_path,
        &endpoint.herdr_session,
    );
    let input = queued_input();
    runtime.block_on(async {
        rig.shared
            .mailbox(channel)
            .replace_queue(
                vec![input.clone()],
                crate::services::discord::queue_persistence_context(
                    &rig.shared,
                    &provider,
                    channel,
                ),
            )
            .await;
        let before = settled_owner_observation(
            "i8069 before restart presence",
            || reading_now(&rig.shared, &provider, channel),
            |reading| matches!(reading.observed.reason, "superseded" | "catching_up"),
        )
        .await;
        assert_eq!(before.observed.activity, Activity::Unknown);
        assert_eq!(before.observed.reason, "session_identity_unresolved");
        assert_eq!(
            before.notice_observation().hold_leaf,
            Some("hosted_active_or_unknown")
        );
        let held = crate::services::discord::mailbox_take_next_automatic_intervention(
            &rig.shared,
            &provider,
            channel,
        )
        .await;
        assert!(held.intervention.is_none() && held.dispatch_lease.is_none());
        assert!(held.persistence_error.is_none());
        assert_eq!(
            mailbox_snapshot(&rig.shared, channel)
                .await
                .intervention_queue
                .len(),
            1
        );
    });
    let passes = PASSES.with(std::cell::Cell::get);
    {
        let _entered = runtime.enter();
        reconnect_restarted_herdr_with_shared(&rig.shared, &provider);
    }
    assert_eq!(
        PASSES.with(std::cell::Cell::get),
        passes + 1,
        "the public restart pass ran"
    );
    runtime.block_on(async {
        let after = settled_owner_observation(
            "i8069 after restart presence",
            || activity_now(&rig.shared, &provider, channel),
            |observed| matches!(observed.reason, "superseded" | "catching_up"),
        )
        .await;
        assert_eq!(
            after.activity,
            Activity::Idle,
            "retired marker held the channel: {after:?}"
        );
        assert_eq!(after.reason, "no_session");
        assert_eq!(
            fixture.current().await,
            HostedRecord::Known(fixture.execution.clone())
        );
        let taken = crate::services::discord::mailbox_take_next_automatic_intervention(
            &rig.shared,
            &provider,
            channel,
        )
        .await;
        assert!(taken.persistence_error.is_none());
        assert!(!taken.has_more);
        let dequeued = taken
            .intervention
            .as_ref()
            .expect("the original queued input was released");
        assert_eq!(dequeued.message_id, input.message_id);
        assert_eq!(dequeued.text, input.text);
        assert_eq!(dequeued.source_message_ids, input.source_message_ids);
        assert!(taken.dispatch_lease.is_some());
        let snapshot = mailbox_snapshot(&rig.shared, channel).await;
        assert_eq!(snapshot.pending_user_dispatch, Some(input.message_id));
        assert!(snapshot.intervention_queue.is_empty());
        let (durable, _) = crate::services::turn_orchestrator::load_channel_pending_queue_for_tests(
            &provider,
            &rig.shared.token_hash,
            channel,
        );
        assert!(
            durable.is_empty(),
            "the drained input must leave the durable queue"
        );
    });
    assert!(trace.calls() > 0, "the real absence inventory gate ran");
    let logical = &fixture.execution.owner.logical_key;
    assert!(!marker_path(logical, tmux_common::TMUX_RUNTIME_KIND_TEMP_EXT).exists());
    assert_eq!(
        std::fs::read_to_string(marker_path(logical, "host_kind")).unwrap(),
        "herdr"
    );
    assert_eq!(
        std::fs::read_to_string(marker_path(logical, "spawn_nonce")).unwrap(),
        fixture.execution.execution_nonce
    );
    assert!(
        socket.requests().is_empty(),
        "retired cleanup must not touch the Herdr pane"
    );
}
