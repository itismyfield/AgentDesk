#[test]
fn intake_dispatch_invariant_direct_execution_body_has_no_external_producer_callsites() {
    for (name, source) in [
        ("intake_gate", include_str!("../intake_gate.rs")),
        ("gateway", include_str!("../../gateway.rs")),
        ("discord_mod", include_str!("../../mod.rs")),
        ("skill", include_str!("../../commands/skill.rs")),
        (
            "text_commands",
            include_str!("../../commands/text_commands.rs"),
        ),
    ] {
        assert_eq!(
            source.matches("handle_text_message(").count(),
            0,
            "{name} bypasses central intake admission"
        );
    }

    for (name, source) in [
        ("intake_gate", include_str!("../intake_gate.rs")),
        ("skill", include_str!("../../commands/skill.rs")),
        (
            "text_commands",
            include_str!("../../commands/text_commands.rs"),
        ),
    ] {
        assert_eq!(
            source.matches("finish_admitted_local(").count(),
            0,
            "{name} bypasses dispatch convenience path"
        );
    }

    assert_eq!(
        include_str!("../intake_gate.rs")
            .matches("dispatch_text_intake(")
            .count(),
        1,
        "regular FullEvent intake lost central dispatch"
    );
    assert_eq!(
        include_str!("../../commands/skill.rs")
            .matches("dispatch_skill_intake(")
            .count(),
        2,
        "unknown and registered slash skills must both use central dispatch"
    );
    assert_eq!(
        include_str!("../../commands/text_commands.rs")
            .matches("dispatch_skill_intake(")
            .count(),
        1,
        "text skills must use central dispatch"
    );
}

#[test]
fn intake_dispatch_invariant_worker_post_claim_is_the_only_router_bypass() {
    // The worker boundary spans the intake body module plus its extracted
    // worker entry seam (intake_turn/worker_entry.rs, split out in #4743).
    let worker_body = include_str!("../message_handler/intake_turn.rs");
    let worker_entry = include_str!("../message_handler/intake_turn/worker_entry.rs");
    for source in [worker_body, worker_entry] {
        assert!(!source.contains("dispatch_text_intake("));
        assert!(!source.contains("admit_text_intake("));
        assert!(!source.contains("try_route_intake("));
        assert!(!source.contains("IntakeSubmission {"));
    }
    assert_eq!(
        worker_body.matches("handle_text_message(").count(),
        1,
        "the worker body module must contain only the body definition"
    );
    assert_eq!(
        worker_entry.matches("handle_text_message(").count(),
        1,
        "the extracted worker entry must contain only its direct post-claim call"
    );
    assert_eq!(
        include_str!("../message_handler.rs")
            .matches("handle_text_message(")
            .count(),
        1,
        "the permit-consuming local adapter must be the sole parent-module body call"
    );
}

#[test]
fn intake_dispatch_invariant_queued_entrypoints_promote_markers_after_admission_before_finish() {
    for (name, source, promotion) in [
        (
            "gateway",
            include_str!("../../gateway.rs"),
            "drain_dispatched_queue_markers(",
        ),
        (
            "discord_mod",
            include_str!("../../mod.rs"),
            "start_and_drain_kickoff_markers(",
        ),
    ] {
        let admit = source
            .find("admit_queued_intake(")
            .unwrap_or_else(|| panic!("{name} queue path lost central admission"));
        let promote = source
            .find(promotion)
            .unwrap_or_else(|| panic!("{name} queue path lost marker promotion"));
        let finish = source
            .find("finish_admitted_queued_intake(")
            .unwrap_or_else(|| panic!("{name} queue path lost admitted local finish"));
        assert!(
            admit < promote && promote < finish,
            "{name} must promote persisted queue markers only after admission and before finish"
        );
        assert_eq!(source.matches(promotion).count(), 1);
        assert_eq!(source.matches("finish_admitted_queued_intake(").count(), 1);
    }
}

use std::ffi::OsString;
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use super::{
    IntakeOrigin, IntakeSubmission, QueuedAdmissionDisposition, dispatch_skill_intake,
    dispatch_text_intake,
};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::router::message_handler::{IntakeDeps, IntakeRequest};
use crate::services::discord::router::{TurnKind, admit_queued_intake};
use crate::services::provider::ProviderKind;
use crate::services::turn_orchestrator::{Intervention, InterventionMode};

struct ScopedIntakeTestEnv {
    _lock: std::sync::MutexGuard<'static, ()>,
    _root: tempfile::TempDir,
    previous_mode: Option<OsString>,
    previous_root: Option<OsString>,
}

impl ScopedIntakeTestEnv {
    fn enforce() -> Self {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().expect("temporary AgentDesk root");
        let previous_mode = std::env::var_os("ADK_INTAKE_ROUTING_MODE");
        let previous_root = std::env::var_os("AGENTDESK_ROOT_DIR");
        // SAFETY: the crate-wide env lock serializes tests that mutate process
        // environment, and Drop restores both variables before releasing it.
        unsafe {
            std::env::set_var("ADK_INTAKE_ROUTING_MODE", "enforce");
            std::env::set_var("AGENTDESK_ROOT_DIR", root.path());
        }
        Self {
            _lock: lock,
            _root: root,
            previous_mode,
            previous_root,
        }
    }
}

impl Drop for ScopedIntakeTestEnv {
    fn drop(&mut self) {
        // SAFETY: this guard still owns the crate-wide env lock.
        unsafe {
            restore_env("ADK_INTAKE_ROUTING_MODE", self.previous_mode.take());
            restore_env("AGENTDESK_ROOT_DIR", self.previous_root.take());
        }
    }
}

unsafe fn restore_env(key: &str, value: Option<OsString>) {
    match value {
        Some(value) => unsafe { std::env::set_var(key, value) },
        None => unsafe { std::env::remove_var(key) },
    }
}

async fn seed_foreign_owner(pool: &sqlx::PgPool, channel_id: ChannelId, owner_instance_id: &str) {
    let channel = channel_id.get().to_string();
    let agent_id = format!("agent-{}", channel_id.get());
    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id,
         preferred_intake_node_labels) VALUES ($1, 'Test', 'claude', $2, '[]'::jsonb)",
    )
    .bind(&agent_id)
    .bind(&channel)
    .execute(pool)
    .await
    .expect("seed agent");
    sqlx::query(
        "INSERT INTO worker_nodes (instance_id, status, role, effective_role,
         labels, capabilities, last_heartbeat_at, started_at, updated_at)
         VALUES ($1, 'online', 'worker', 'worker', '[]'::jsonb,
         -- A real foreign worker always advertises \"preserve_on_cancel_v1\" via
         -- capabilities_with_runtime_state() (intake_worker_capabilities.rs). Without
         -- it, node_supports_intake_request() treats the node as protocol-incompatible
         -- for preserve_on_cancel=true requests, and resolve_session_owner() classifies
         -- it as LiveForeignIncompatible instead of LiveForeign, blocking the forward
         -- entirely (#4550 multinode preserve tri-state).
         '{\"intake_worker\":{\"enabled\":true,\"providers\":[\"claude\"],\"features\":[\"preserve_on_cancel_v1\"]}}'::jsonb,
         NOW(), NOW(), NOW())",
    )
    .bind(owner_instance_id)
    .execute(pool)
    .await
    .expect("seed worker owner");
    sqlx::query(
        "INSERT INTO sessions (session_key, agent_id, provider, channel_id,
         instance_id, status, last_heartbeat)
         VALUES ($1, $2, 'claude', $3, $4, 'idle', NOW())",
    )
    .bind(format!("claude-{channel}"))
    .bind(agent_id)
    .bind(channel)
    .bind(owner_instance_id)
    .execute(pool)
    .await
    .expect("seed session owner");
}

fn request(channel_id: ChannelId, message_id: u64, text: &str) -> IntakeRequest {
    IntakeRequest {
        channel_id,
        user_msg_id: MessageId::new(message_id),
        request_owner: UserId::new(4350),
        request_owner_name: "owner-affinity-test".to_string(),
        user_text: text.to_string(),
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        merge_consecutive: false,
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: TurnKind::Foreground,
        preserve_on_cancel: false,
    }
}

fn queued_intervention(message_id: u64, pending_uploads: Vec<String>) -> Intervention {
    let queued_generation = crate::services::discord::runtime_store::process_generation();
    Intervention {
        author_id: UserId::new(4350),
        author_is_bot: false,
        message_id: MessageId::new(message_id),
        queued_generation,
        source_message_ids: vec![MessageId::new(message_id)],
        // A genuine human-authored queued message always carries a
        // `user_instruction` source marker from the enqueue path (see
        // intake_gate/queue_effects.rs), so `preserve_on_cancel()` is true.
        // Mirror that here instead of an empty vec so the forwarded outbox
        // row records `Some(true)` for the multinode preserve tri-state (#4550).
        source_message_queued_generations: vec![
            crate::services::turn_orchestrator::SourceMessageQueuedGeneration::user_instruction(
                MessageId::new(message_id),
                queued_generation,
            ),
        ],
        source_text_segments: Vec::new(),
        text: format!("queued-{message_id}"),
        mode: InterventionMode::Soft,
        created_at: std::time::Instant::now(),
        reply_context: None,
        has_reply_boundary: false,
        merge_consecutive: false,
        pending_uploads,
        voice_announcement: None,
    }
}

fn deps<'a>(
    http: &'a Arc<serenity::Http>,
    shared: &'a Arc<crate::services::discord::SharedData>,
) -> IntakeDeps<'a> {
    IntakeDeps {
        http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared,
        token: "Bot intake-dispatch-test",
    }
}

async fn mark_open_routes_done(pool: &sqlx::PgPool, channel_id: ChannelId) {
    sqlx::query(
        "UPDATE intake_outbox SET status = 'done', completed_at = NOW()
         WHERE channel_id = $1 AND status IN ('pending', 'claimed', 'accepted', 'spawned')",
    )
    .bind(channel_id.get().to_string())
    .execute(pool)
    .await
    .expect("finish prior open route");
}

#[tokio::test(flavor = "current_thread")]
async fn intake_dispatch_invariant_enforce_without_postgres_blocks_owner_unknown() {
    let _env = ScopedIntakeTestEnv::enforce();
    let shared = crate::services::discord::make_shared_data_for_tests();
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let submission = IntakeSubmission {
        provider: ProviderKind::Claude,
        request: request(ChannelId::new(4_350_001), 4_350_011, "owner unknown"),
        origin: IntakeOrigin::LiveMessage,
        preserve_on_cancel: false,
        has_nonportable_uploads: false,
        preloaded_uploads: Vec::new(),
        voice_announcement: None,
    };

    assert!(matches!(
        super::admit_text_intake(&deps, &submission).await,
        super::IntakeAdmission::Blocked {
            reason: crate::services::cluster::intake_router_hook::IntakeBlockedReason::RoutingDependencyFailed { .. }
        }
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn live_and_skill_producers_forward_to_foreign_owner_pg() {
    let _env = ScopedIntakeTestEnv::enforce();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let channel_id = ChannelId::new(4_350_101);
    let owner = "worker-owner-4350-live";
    seed_foreign_owner(&pool, channel_id, owner).await;

    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);

    dispatch_text_intake(
        &deps,
        IntakeSubmission {
            provider: ProviderKind::Claude,
            request: request(channel_id, 4_350_111, "plain live intake"),
            origin: IntakeOrigin::LiveMessage,
            preserve_on_cancel: true,
            has_nonportable_uploads: false,
            preloaded_uploads: Vec::new(),
            voice_announcement: None,
        },
    )
    .await
    .expect("plain intake forwards");
    mark_open_routes_done(&pool, channel_id).await;

    dispatch_skill_intake(
        &deps,
        ProviderKind::Claude,
        channel_id,
        MessageId::new(4_350_112),
        UserId::new(4350),
        "slash-owner".to_string(),
        "/unknown-skill".to_string(),
        IntakeOrigin::SlashSkill,
        Vec::new(),
    )
    .await
    .expect("slash skill forwards");
    mark_open_routes_done(&pool, channel_id).await;

    dispatch_skill_intake(
        &deps,
        ProviderKind::Claude,
        channel_id,
        MessageId::new(4_350_113),
        UserId::new(4350),
        "text-owner".to_string(),
        "Execute /registered-skill".to_string(),
        IntakeOrigin::TextSkill,
        Vec::new(),
    )
    .await
    .expect("text skill forwards");

    let rows: Vec<(String, String, Option<bool>)> = sqlx::query_as(
        "SELECT target_instance_id, provider, preserve_on_cancel FROM intake_outbox
         WHERE channel_id = $1 ORDER BY id",
    )
    .bind(channel_id.get().to_string())
    .fetch_all(&pool)
    .await
    .expect("load forwarded rows");
    assert_eq!(
        rows,
        vec![
            (owner.to_string(), "claude".to_string(), Some(true)),
            (owner.to_string(), "claude".to_string(), Some(false)),
            (owner.to_string(), "claude".to_string(), Some(false)),
        ]
    );
    assert!(
        shared.core.lock().await.sessions.is_empty(),
        "foreign admission must not create a local session"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn queued_foreign_owner_forwards_without_local_body_pg() {
    let _env = ScopedIntakeTestEnv::enforce();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let channel_id = ChannelId::new(4_350_201);
    let owner = "worker-owner-4350-queue";
    seed_foreign_owner(&pool, channel_id, owner).await;

    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let intervention = queued_intervention(4_350_211, Vec::new());
    let admitted = match admit_queued_intake(
        &deps,
        ProviderKind::Claude,
        channel_id,
        &intervention,
        intervention.author_id,
        "queue-owner".to_string(),
        false,
        false,
        "owner_affinity_queue_test",
        None,
    )
    .await
    {
        QueuedAdmissionDisposition::Admitted(admitted) => admitted,
        QueuedAdmissionDisposition::Deferred
        | QueuedAdmissionDisposition::RejectedNonPortableAttachment
        | QueuedAdmissionDisposition::RejectedRestore => {
            panic!("live foreign owner should forward")
        }
    };
    super::finish_admitted_queued_intake(&deps, admitted, &intervention)
        .await
        .expect("forwarded queued finish is a no-op");

    let row: (String, String, Option<bool>) = sqlx::query_as(
        "SELECT target_instance_id, provider, preserve_on_cancel
         FROM intake_outbox WHERE channel_id = $1",
    )
    .bind(channel_id.get().to_string())
    .fetch_one(&pool)
    .await
    .expect("forwarded queue row");
    assert_eq!(row, (owner.to_string(), "claude".to_string(), Some(true)));
    assert!(shared.core.lock().await.sessions.is_empty());
    assert!(
        shared
            .mailbox(channel_id)
            .snapshot()
            .await
            .intervention_queue
            .is_empty(),
        "forwarded item is consumed instead of requeued"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn queued_foreign_attachment_is_rejected_without_requeue_pg() {
    let _env = ScopedIntakeTestEnv::enforce();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let channel_id = ChannelId::new(4_350_301);
    seed_foreign_owner(&pool, channel_id, "worker-owner-4350-attachment").await;

    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let local_path = "/private/tmp/gateway-local-attachment.txt".to_string();
    let intervention = queued_intervention(4_350_311, vec![local_path.clone()]);

    assert!(matches!(
        admit_queued_intake(
            &deps,
            ProviderKind::Claude,
            channel_id,
            &intervention,
            intervention.author_id,
            "attachment-owner".to_string(),
            false,
            false,
            "owner_affinity_attachment_test",
            None,
        )
        .await,
        QueuedAdmissionDisposition::RejectedNonPortableAttachment
    ));

    let snapshot = shared.mailbox(channel_id).snapshot().await;
    assert!(
        snapshot.intervention_queue.is_empty(),
        "a nonportable queued attachment must be consumed, not requeued forever"
    );
    assert!(
        shared.core.lock().await.sessions.is_empty(),
        "foreign attachment path must never enter local session state"
    );
    let outbox_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
            .bind(channel_id.get().to_string())
            .fetch_one(&pool)
            .await
            .expect("count attachment routes");
    assert_eq!(outbox_count, 0);
    assert_eq!(
        shared
            .restart
            .deferred_hook_backlog
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a rejected attachment must not arm a retry backstop"
    );

    pool.close().await;
    pg_db.drop().await;
}

#[tokio::test(flavor = "current_thread")]
async fn distinct_open_route_requeues_queued_successor_pg() {
    let _env = ScopedIntakeTestEnv::enforce();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let channel_id = ChannelId::new(4_350_401);
    let owner = "worker-owner-4350-open-route";
    seed_foreign_owner(&pool, channel_id, owner).await;

    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    dispatch_text_intake(
        &deps,
        IntakeSubmission {
            provider: ProviderKind::Claude,
            request: request(channel_id, 4_350_411, "predecessor"),
            origin: IntakeOrigin::LiveMessage,
            preserve_on_cancel: false,
            has_nonportable_uploads: false,
            preloaded_uploads: Vec::new(),
            voice_announcement: None,
        },
    )
    .await
    .expect("predecessor forwards");

    let successor = queued_intervention(4_350_412, Vec::new());
    let persistence = crate::services::discord::queue_persistence_context(
        &shared,
        &ProviderKind::Claude,
        channel_id,
    );
    shared
        .mailbox(channel_id)
        .replace_queue(vec![successor.clone()], persistence.clone())
        .await;
    let dequeued = shared.mailbox(channel_id).take_next_soft(persistence).await;
    let intervention = dequeued
        .intervention
        .expect("queued successor must be dequeued before admission");
    assert!(matches!(
        admit_queued_intake(
            &deps,
            ProviderKind::Claude,
            channel_id,
            &intervention,
            intervention.author_id,
            "successor-owner".to_string(),
            false,
            false,
            "owner_affinity_open_route_test",
            dequeued.dispatch_lease,
        )
        .await,
        QueuedAdmissionDisposition::Deferred
    ));

    let snapshot = shared.mailbox(channel_id).snapshot().await;
    assert_eq!(snapshot.intervention_queue.len(), 1);
    assert_eq!(
        snapshot.intervention_queue[0].message_id,
        successor.message_id
    );
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT target_instance_id, user_msg_id FROM intake_outbox
         WHERE channel_id = $1 ORDER BY id",
    )
    .bind(channel_id.get().to_string())
    .fetch_all(&pool)
    .await
    .expect("load open-route rows");
    assert_eq!(rows, vec![(owner.to_string(), "4350411".to_string())]);
    assert_eq!(
        shared
            .restart
            .deferred_hook_backlog
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    pool.close().await;
    pg_db.drop().await;
}
