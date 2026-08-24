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
    IntakeAdmission, IntakeOrigin, IntakeSubmission, QueuedAdmissionDisposition,
    admission_for_decision, dispatch_skill_intake, dispatch_text_intake,
};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox_status::IntakeOutboxStatus;
use crate::services::cluster::intake_router_hook::{
    IntakeRouterContext, IntakeRouterDecision, IntakeRoutingMode, ResolvedSessionOwner,
    try_route_intake,
};
use crate::services::cluster::intake_routing_config::OwnerAuthorityChannelOptIn;
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
        intake_outbox_id: None,
        channel_id,
        user_msg_id: MessageId::new(message_id),
        source_message_ids: Vec::new(),
        busy_followup_retry_user_msg_id: MessageId::new(message_id),
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

fn submission_for_admission(channel_id: ChannelId, message_id: u64) -> IntakeSubmission {
    IntakeSubmission {
        provider: ProviderKind::Claude,
        request: request(channel_id, message_id, "admission policy"),
        origin: IntakeOrigin::LiveMessage,
        preserve_on_cancel: false,
        has_nonportable_uploads: false,
        attachments: Vec::new(),
        preloaded_uploads: Vec::new(),
        voice_announcement: None,
    }
}

#[test]
fn telemetry_only_unopted_live_local_pending_open_route_runs_locally_5040() {
    let submission = submission_for_admission(ChannelId::new(4_350_351), 4_350_361);
    let admission = admission_for_decision(
        OwnerAuthorityChannelOptIn::NotOptedIn,
        12,
        IntakeRouterDecision::DeferredOpenRoute {
            target_instance_id: "mac-mini-release".to_string(),
            open_route_id: None,
            open_route_status: Some(IntakeOutboxStatus::Pending),
            open_route_age_secs: Some(60),
            resolved_owner: ResolvedSessionOwner::LiveLocal,
        },
        &submission,
    );

    assert!(
        matches!(admission, IntakeAdmission::Local(_)),
        "an explicitly unlisted local pending route may use the stale-route recovery exception"
    );
}

#[test]
fn telemetry_only_unopted_live_local_fresh_pending_route_stays_fenced_5040() {
    let submission = submission_for_admission(ChannelId::new(4_350_365), 4_350_375);
    let admission = admission_for_decision(
        OwnerAuthorityChannelOptIn::NotOptedIn,
        12,
        IntakeRouterDecision::DeferredOpenRoute {
            target_instance_id: "mac-mini-release".to_string(),
            open_route_id: None,
            open_route_status: Some(IntakeOutboxStatus::Pending),
            open_route_age_secs: Some(1),
            resolved_owner: ResolvedSessionOwner::LiveLocal,
        },
        &submission,
    );

    assert!(matches!(
        admission,
        IntakeAdmission::DeferredOpenRoute { .. }
    ));
}

#[test]
fn telemetry_only_unopted_live_foreign_owner_stays_fenced_5040() {
    let submission = submission_for_admission(ChannelId::new(4_350_371), 4_350_381);
    let admission = admission_for_decision(
        OwnerAuthorityChannelOptIn::NotOptedIn,
        12,
        IntakeRouterDecision::DeferredOpenRoute {
            target_instance_id: "foreign-instance".to_string(),
            open_route_id: None,
            open_route_status: Some(IntakeOutboxStatus::Pending),
            open_route_age_secs: Some(60),
            resolved_owner: ResolvedSessionOwner::LiveForeign,
        },
        &submission,
    );

    assert!(
        matches!(
            admission,
            IntakeAdmission::DeferredOpenRoute {
                ref target_instance_id,
            } if target_instance_id == "foreign-instance"
        ),
        "a live foreign owner must retain the open-route fence"
    );
}

#[test]
fn telemetry_only_unopted_unknown_owner_authority_keeps_local_fence_5040() {
    let submission = submission_for_admission(ChannelId::new(4_350_391), 4_350_401);
    let admission = admission_for_decision(
        OwnerAuthorityChannelOptIn::Unknown,
        12,
        IntakeRouterDecision::DeferredOpenRoute {
            target_instance_id: "local-instance".to_string(),
            open_route_id: None,
            open_route_status: Some(IntakeOutboxStatus::Pending),
            open_route_age_secs: Some(60),
            resolved_owner: ResolvedSessionOwner::LiveLocal,
        },
        &submission,
    );

    assert!(matches!(
        admission,
        IntakeAdmission::DeferredOpenRoute {
            ref target_instance_id,
        } if target_instance_id == "local-instance"
    ));
}

#[test]
fn telemetry_only_unopted_local_accepted_route_stays_fenced_5040() {
    let submission = submission_for_admission(ChannelId::new(4_350_411), 4_350_421);
    let admission = admission_for_decision(
        OwnerAuthorityChannelOptIn::NotOptedIn,
        12,
        IntakeRouterDecision::DeferredOpenRoute {
            target_instance_id: "local-instance".to_string(),
            open_route_id: None,
            open_route_status: Some(IntakeOutboxStatus::Accepted),
            open_route_age_secs: Some(60),
            resolved_owner: ResolvedSessionOwner::LiveLocal,
        },
        &submission,
    );

    assert!(matches!(
        admission,
        IntakeAdmission::DeferredOpenRoute {
            ref target_instance_id,
        } if target_instance_id == "local-instance"
    ));

    let unavailable = admission_for_decision(
        OwnerAuthorityChannelOptIn::NotOptedIn,
        12,
        IntakeRouterDecision::DeferredOpenRoute {
            target_instance_id: "local-instance".to_string(),
            open_route_id: None,
            open_route_status: None,
            open_route_age_secs: Some(60),
            resolved_owner: ResolvedSessionOwner::LiveLocal,
        },
        &submission,
    );
    assert!(matches!(
        unavailable,
        IntakeAdmission::DeferredOpenRoute { .. }
    ));
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
    let query = format!(
        "UPDATE intake_outbox SET status = 'done', completed_at = NOW()
         WHERE channel_id = $1 AND status IN ({})",
        crate::db::intake_outbox_open_status::INTAKE_OUTBOX_OPEN_STATUSES_SQL
    );
    sqlx::query(&query)
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
        attachments: Vec::new(),
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
            attachments: Vec::new(),
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
        None,
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
        None,
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
async fn raw_attachment_foreign_owner_blocks_before_outbox_or_local_state_pg() {
    let _env = ScopedIntakeTestEnv::enforce();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let channel_id = ChannelId::new(4_350_151);
    seed_foreign_owner(&pool, channel_id, "worker-owner-4350-raw-attachment").await;

    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let http = Arc::new(serenity::Http::new("Bot intake-dispatch-test"));
    let deps = deps(&http, &shared);
    let submission = IntakeSubmission {
        provider: ProviderKind::Claude,
        request: request(channel_id, 4_350_161, "inspect the attachment"),
        origin: IntakeOrigin::LiveMessage,
        preserve_on_cancel: false,
        has_nonportable_uploads: false,
        attachments: vec![super::super::message_handler::AttachmentDescriptor {
            filename: "report.txt".to_string(),
            url: "https://cdn.discordapp.com/attachments/1/2/report.txt".to_string(),
        }],
        preloaded_uploads: Vec::new(),
        voice_announcement: None,
    };

    assert!(matches!(
        super::admit_text_intake(&deps, &submission).await,
        super::IntakeAdmission::Blocked {
            reason: crate::services::cluster::intake_router_hook::IntakeBlockedReason::NonPortableAttachmentForeignOwner { .. }
        }
    ));
    let outbox_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM intake_outbox WHERE channel_id = $1")
            .bind(channel_id.get().to_string())
            .fetch_one(&pool)
            .await
            .expect("count raw attachment routes");
    assert_eq!(
        outbox_count, 0,
        "raw attachments never enter a foreign outbox"
    );
    assert!(
        shared.core.lock().await.sessions.is_empty(),
        "blocked raw attachment admission must not create local session state"
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
            attachments: Vec::new(),
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

#[tokio::test(flavor = "current_thread")]
async fn dispatched_open_route_never_uses_stale_local_recovery_pg() {
    let _env = ScopedIntakeTestEnv::enforce();
    let pg_db = TestPostgresDb::create().await;
    let pool = pg_db.connect_and_migrate().await;
    let channel_id = ChannelId::new(4_350_451);
    let channel = channel_id.get().to_string();
    let self_instance =
        crate::services::cluster::node_registry::resolve_self_instance_id_without_config();

    sqlx::query(
        "INSERT INTO agents (id, name, provider, discord_channel_id)
         VALUES ('agent-local-dispatched', 'Local', 'claude', $1)",
    )
    .bind(&channel)
    .execute(&pool)
    .await
    .expect("seed local agent");
    sqlx::query(
        "INSERT INTO sessions (session_key, agent_id, provider, channel_id,
         instance_id, status, last_heartbeat)
         VALUES ('claude-local-dispatched', 'agent-local-dispatched', 'claude',
         $1, $2, 'idle', NOW())",
    )
    .bind(&channel)
    .bind(&self_instance)
    .execute(&pool)
    .await
    .expect("seed live local owner");
    let route_id: i64 = sqlx::query_scalar(
        "INSERT INTO intake_outbox (
            target_instance_id, forwarded_by_instance_id, required_labels,
            channel_id, user_msg_id, request_owner_id, user_text,
            turn_kind, agent_id, provider, status, attempt_no, created_at, dispatched_at
         ) VALUES ($1, 'leader-1', '[]'::JSONB, $2, 'msg-dispatched', '50',
            'prior', 'foreground', 'agent-local-dispatched', 'claude',
            'dispatched', 1, NOW() - INTERVAL '60 seconds',
            NOW() - INTERVAL '60 seconds')
         RETURNING id",
    )
    .bind(&self_instance)
    .bind(&channel)
    .fetch_one(&pool)
    .await
    .expect("seed stale dispatched route");

    let submission = submission_for_admission(channel_id, 4_350_452);
    let request_owner_id = submission.request.request_owner.get().to_string();
    let ctx = IntakeRouterContext {
        mode: IntakeRoutingMode::Enforce,
        leader_instance_id: &self_instance,
        provider: "claude",
        channel_id: &channel,
        user_msg_id: "4350452",
        request_owner_id: &request_owner_id,
        request_owner_name: Some(&submission.request.request_owner_name),
        user_text: &submission.request.user_text,
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: Some(false),
        turn_kind: "foreground",
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        node_override_instance_id: None,
        has_nonportable_uploads: false,
    };
    let decision = try_route_intake(&pool, &ctx).await;
    assert!(matches!(
        &decision,
        IntakeRouterDecision::DeferredOpenRoute {
            open_route_id: Some(id),
            open_route_status: Some(IntakeOutboxStatus::Dispatched),
            resolved_owner: ResolvedSessionOwner::LiveLocal,
            ..
        } if *id == route_id
    ));
    let admission = admission_for_decision(
        OwnerAuthorityChannelOptIn::NotOptedIn,
        12,
        decision,
        &submission,
    );
    assert!(matches!(
        admission,
        IntakeAdmission::DeferredOpenRoute { .. }
    ));

    let unchanged: (String, Option<String>) =
        sqlx::query_as("SELECT status, last_error FROM intake_outbox WHERE id = $1")
            .bind(route_id)
            .fetch_one(&pool)
            .await
            .expect("read dispatched route after fenced admission");
    assert_eq!(unchanged, ("dispatched".to_string(), None));

    pool.close().await;
    pg_db.drop().await;
}

#[cfg(unix)]
#[rustfmt::skip]
async fn insert_raw_delivery_journal_row(connection: &mut sqlx::PgConnection, obligation: uuid::Uuid, attempt: Option<uuid::Uuid>, kind: &str, seq: i16, payload: serde_json::Value, receipt: [Option<&str>; 3]) {
    sqlx::query(
        "INSERT INTO delivery_journal_events
         (event_id,obligation_id,attempt_id,event_kind,event_seq,idempotency_key,canonical_payload,
          requested_channel_id,returned_channel_id,message_id) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)",
    ).bind(uuid::Uuid::new_v4()).bind(obligation).bind(attempt).bind(kind).bind(seq)
        .bind(vec![seq as u8]).bind(payload).bind(receipt[0]).bind(receipt[1]).bind(receipt[2])
        .execute(&mut *connection).await.expect("seed raw journal row");
}

#[cfg(unix)]
#[rustfmt::skip]
#[tokio::test(flavor = "current_thread")]
async fn obligation_window_facade_restores_sequence_and_receipt_pg() {
    let db=TestPostgresDb::create().await; let pool=db.connect_and_migrate().await; let mut transaction=pool.begin().await.expect("begin caller transaction");
    let obligation=uuid::Uuid::new_v4(); let attempt=uuid::Uuid::new_v4();
    insert_raw_delivery_journal_row(&mut transaction,obligation,Some(attempt),"C",3,serde_json::json!({"frontier_start":10,"frontier_end":20}),[None,None,None]).await;
    insert_raw_delivery_journal_row(&mut transaction,obligation,None,"O",0,serde_json::json!({"intake_outbox_id":42}),[None,None,None]).await;
    insert_raw_delivery_journal_row(&mut transaction,obligation,Some(attempt),"T",2,serde_json::json!({"requested_channel_id":"10","returned_channel_id":"10","message_id":"20"}),[Some("10"),Some("10"),Some("20")]).await;
    insert_raw_delivery_journal_row(&mut transaction,obligation,Some(attempt),"A",1,serde_json::json!({"frontier_start":10,"frontier_end":20}),[None,None,None]).await;
    let judgment=crate::services::discord::session_relay_sink::journal::judge_obligation_window(&mut transaction,obligation).await.expect("read uncommitted raw window");
    assert_eq!(judgment.delivered_outbox_id(),Some(42)); assert!(!judgment.malformed());
    transaction.rollback().await.expect("rollback caller transaction"); pool.close().await; db.drop().await;
}

#[cfg(unix)]
#[rustfmt::skip]
#[tokio::test(flavor = "current_thread")]
async fn obligation_window_facade_rejects_unknown_kind_pg() {
    let db=TestPostgresDb::create().await; let pool=db.connect_and_migrate().await; let mut connection=pool.acquire().await.expect("acquire fixture connection");
    sqlx::query("ALTER TABLE delivery_journal_events DROP CONSTRAINT delivery_journal_kind_check, DROP CONSTRAINT delivery_journal_slot_check, DROP CONSTRAINT delivery_journal_attempt_check").execute(&pool).await.expect("permit unknown-kind raw fixture");
    let obligation=uuid::Uuid::new_v4();
    insert_raw_delivery_journal_row(&mut connection,obligation,None,"future",9,serde_json::json!({}),[None,None,None]).await;
    let judgment=crate::services::discord::session_relay_sink::journal::judge_obligation_window(&mut connection,obligation).await.expect("read unknown-kind window");
    assert_eq!(judgment.delivered_outbox_id(),None); assert!(judgment.malformed());
    drop(connection); pool.close().await; db.drop().await;
}
