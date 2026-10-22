use super::*;
use crate::config::session_hosts::{self, EndpointMode};
use crate::services::discord::{make_shared_data_for_tests, router::herdr_consent};
use crate::services::provider::ProviderKind;
use crate::services::session_host::{
    ServerWitness,
    policy::consent::{ConsentKey, IntentRef, Preview, Question},
};
use crate::services::turn_orchestrator::{
    Intervention, InterventionMode, QueueExitEvent, QueueExitKind, QueuePersistenceContext,
    SourceMessageQueuedGeneration,
};
use poise::serenity_prelude::{ChannelId, UserId};

const CHANNEL: u64 = 98_701;
const SOURCE: u64 = 98_702;
const GENERATION: u64 = 7;

fn question() -> Question {
    Question {
        key: ConsentKey {
            node: "node".into(),
            user: 501,
            socket: "/adk/herdr/agentdesk.sock".into(),
            named_session: "agentdesk".into(),
            config: "/fixture/config.toml".into(),
            target: "/fixture/target.toml".into(),
        },
        preview: Preview {
            revision: 1,
            witness: ServerWitness::for_test(7),
            target_hash: [1; 32],
            chain_hash: [2; 32],
            checker_hash: [3; 32],
        },
        owner: 1,
        bot: 2,
        provider: "claude".into(),
        channel: CHANNEL,
        message: 4,
        intent: IntentRef {
            store: "mailbox".into(),
            id: format!("{SOURCE}:{GENERATION}"),
            provider: "claude".into(),
            channel: CHANNEL,
            source_message: SOURCE,
            fifo: GENERATION,
        },
    }
}

fn input() -> Intervention {
    Intervention {
        author_id: UserId::new(1),
        author_is_bot: false,
        message_id: MessageId::new(SOURCE),
        queued_generation: GENERATION,
        source_message_ids: vec![MessageId::new(SOURCE)],
        source_message_queued_generations: vec![SourceMessageQueuedGeneration::user_instruction(
            MessageId::new(SOURCE),
            GENERATION,
        )],
        source_text_segments: Vec::new(),
        text: "original held input".into(),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads: Vec::new(),
        voice_announcement: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn clear_consent_settles_only_exact_durable_waiting_refs_and_keeps_unknowns() {
    for case in [
        "confirmed",
        "failed",
        "unknown",
        "not_waiting",
        "wrong_generation",
        "requeued",
        "outbox",
    ] {
        let _root = crate::config::TestRuntimeRootGuard::new();
        let _boot = session_hosts::force_for_test(Some("node"), &[(CHANNEL, "node")])
            .with_mode(EndpointMode::UserDefault);
        let shared = make_shared_data_for_tests();
        let channel = ChannelId::new(CHANNEL);
        let provider = ProviderKind::Claude;
        let persistence = QueuePersistenceContext::new(&provider, &shared.token_hash, None);
        let mailbox = shared.mailbox(channel);
        let queued = mailbox.enqueue(input(), persistence.clone()).await;
        assert!(
            queued.enqueued && queued.persistence_error.is_none(),
            "clear fixture initial enqueue {case}: refusal={:?}, persistence={:?}",
            queued.refusal_reason,
            queued.persistence_error
        );
        let store = herdr_consent::store();
        let mut q = question();
        q.key.target = format!("/fixture/{case}.toml").into();
        if case == "outbox" {
            q.intent.store = "intake_outbox".into();
        }
        if case == "wrong_generation" {
            q.intent.fifo += 1;
            q.intent.id = format!("{SOURCE}:{}", q.intent.fifo);
        }
        let held = store.hold(q, 100).unwrap().into_consent();
        let mut cleared = if case == "requeued" {
            // A stale clear receipt must not settle the source currently present in live and durable custody.
            ClearChannelResult {
                queue_exit_events: vec![QueueExitEvent {
                    intervention: input(),
                    kind: QueueExitKind::Superseded,
                }],
                discarded_message_ids: vec![MessageId::new(SOURCE)],
                ..ClearChannelResult::default()
            }
        } else {
            mailbox.clear(persistence).await
        };
        assert!(cleared.persistence.failed.is_empty());
        assert!(!cleared.persistence.count_unknown);
        match case {
            "failed" => cleared.persistence.failed.push("queue"),
            "unknown" => cleared.persistence.count_unknown = true,
            "not_waiting" => cleared.persistence.not_waiting.push(MessageId::new(SOURCE)),
            _ => {}
        }
        let mut report = ClearReport::default();
        report.absorb(&cleared);
        report
            .settle_consents(&shared, &provider, channel, &cleared)
            .await;
        let current = store.load(&held.key).unwrap().unwrap();
        assert_eq!(
            current.unresolved_intents().is_empty(),
            case == "confirmed",
            "only confirmed exact removal settles: {case}"
        );
        if case == "confirmed" {
            report
                .settle_consents(&shared, &provider, channel, &cleared)
                .await;
            assert_eq!(
                store.load(&held.key).unwrap().unwrap().settlements.len(),
                1,
                "repeated clear settlement is idempotent"
            );
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn legacy_actual_clear_ignores_unreadable_consent_storage() {
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _boot = session_hosts::force_for_test(Some("node"), &[(CHANNEL, "node")]);
    let directory = crate::config::runtime_root()
        .unwrap()
        .join("runtime/herdr_policy/consents");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join("unknown.json");
    std::fs::write(&path, b"unreadable consent snapshot").unwrap();
    let shared = make_shared_data_for_tests();
    let channel = ChannelId::new(CHANNEL);
    super::super::clear_persist_failure_tests::seed_session(&shared, channel).await;
    let persistence = QueuePersistenceContext::new(&ProviderKind::Claude, &shared.token_hash, None);
    let queued = shared.mailbox(channel).enqueue(input(), persistence).await;
    assert!(queued.enqueued && queued.persistence_error.is_none());
    let http = std::sync::Arc::new(poise::serenity_prelude::Http::new(""));
    let report = super::super::clear_channel_session_state(
        &http,
        &shared,
        &ProviderKind::Claude,
        channel,
        "/clear",
        super::super::SoftClearNotifyMode::Suppress,
    )
    .await
    .unwrap();
    assert!(
        !report.failed.contains(&"consent_discard"),
        "Legacy clear must not read malformed consent storage"
    );
    assert!(
        report
            .discarded
            .as_ref()
            .unwrap()
            .contains(&MessageId::new(SOURCE))
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"unreadable consent snapshot"
    );
}

async fn actual_user_default_clear_settlement(retired: bool) {
    use crate::db::auto_queue::test_support::TestPostgresDb;
    use crate::db::dispatched_sessions::hosted_execution::{
        HostedExecution, HostedOwner, HostedState, SourceRef,
    };
    let _root = crate::config::TestRuntimeRootGuard::new();
    let _boot = session_hosts::force_for_test(Some("node"), &[(CHANNEL, "node")])
        .with_mode(EndpointMode::UserDefault);
    let db = TestPostgresDb::create().await;
    let pool = db.connect_and_migrate().await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let channel = ChannelId::new(CHANNEL);
    let provider = ProviderKind::Claude;
    super::super::clear_persist_failure_tests::seed_session(&shared, channel).await;
    let name = provider.build_tmux_session_name("adk-6233-clear-persist");
    let _tmux = crate::services::provider_teardown::tests::test_support::FakeTmux::install(&name);
    let logical = crate::services::discord::adk_session::build_adk_session_key(
        &shared, channel, &provider, None,
    )
    .await
    .unwrap();
    if retired {
        let owner = HostedOwner {
            provider: "claude".into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: CHANNEL.to_string(),
            logical_key: logical.clone(),
            owner_node: "node".into(),
            runtime_root: "/fixture/runtime".into(),
        };
        let mut hosted = HostedExecution::pending(
            owner.clone(),
            "retired-clear-fixture".into(),
            SourceRef {
                runtime_root: owner.runtime_root.clone(),
                channel: owner.channel_id.clone(),
                provider: owner.provider.clone(),
                logical_key: logical.clone(),
                execution_nonce: "retired-clear-fixture".into(),
                initial_source: None,
                baseline_event_seq: None,
            },
        );
        hosted.state = HostedState::Retired;
        sqlx::query("INSERT INTO sessions (session_key,provider,status,identity_kind,discord_token_hash,channel_id,hosted_execution) VALUES ($1,'claude','idle','discord_channel',$2,$3,$4)")
            .bind(&logical).bind(&shared.token_hash).bind(CHANNEL.to_string()).bind(serde_json::to_value(hosted).unwrap())
            .execute(&pool).await.unwrap();
    }
    let persistence = QueuePersistenceContext::new(&provider, &shared.token_hash, None);
    let queued = shared.mailbox(channel).enqueue(input(), persistence).await;
    assert!(
        queued.enqueued && queued.persistence_error.is_none(),
        "actual_clear_durable_fixture_contract"
    );
    let store = herdr_consent::store();
    let held = store.hold(question(), 100).unwrap().into_consent();
    let mut sentinel_question = question();
    sentinel_question.key.target = "/fixture/other-generation.toml".into();
    sentinel_question.intent.fifo += 1;
    sentinel_question.intent.id = format!("{SOURCE}:{}", sentinel_question.intent.fifo);
    let sentinel = store.hold(sentinel_question, 100).unwrap().into_consent();
    let http = std::sync::Arc::new(poise::serenity_prelude::Http::new(""));
    let report = super::super::clear_channel_session_state(
        &http,
        &shared,
        &provider,
        channel,
        "/clear",
        super::super::SoftClearNotifyMode::Suppress,
    )
    .await
    .unwrap();
    assert!(
        !report.failed.contains(&"consent_discard"),
        "actual_clear_consent_settlement_has_no_failure_contract"
    );
    assert_eq!(
        report.execution.is_some(),
        retired,
        "actual_clear_expected_public_branch_contract"
    );
    let settled = store.load(&held.key).unwrap().unwrap();
    assert_eq!(
        settled.settlements.len(),
        1,
        "actual_clear_entry_settles_exact_ref_once_contract: retired={retired}"
    );
    assert!(
        settled.unresolved_intents().is_empty(),
        "actual_clear_entry_releases_removed_ref_contract"
    );
    assert_eq!(
        store.load(&sentinel.key).unwrap().unwrap(),
        sentinel,
        "actual_clear_entry_preserves_other_generation_contract"
    );
    pool.close().await;
    db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn user_default_actual_general_clear_settles_exact_consent_once_pg() {
    actual_user_default_clear_settlement(false).await;
}

#[tokio::test(flavor = "current_thread")]
async fn user_default_actual_herdr_clear_early_return_settles_exact_consent_once_pg() {
    actual_user_default_clear_settlement(true).await;
}
