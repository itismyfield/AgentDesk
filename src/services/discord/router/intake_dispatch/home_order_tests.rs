//! Ordinary foreign delivery preserves complete source metadata and replay receipts.
use std::ffi::OsString;
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use super::{IntakeAdmission, IntakeOrigin, IntakeSubmission, admit_text_intake};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::router::TurnKind;
use crate::services::discord::router::message_handler::{IntakeDeps, IntakeRequest};
use crate::services::provider::ProviderKind;

const DELEGATED: u64 = 4_380_601;
const OTHER: u64 = 4_380_603;
const GW: &str = "gw-4386";
const MINI: &str = "mini-4386";

/// Enforce routing over a fresh root, holding the crate-wide env lock.
struct EnforceEnv {
    _live: crate::config_live_reload::TestLiveGuard,
    _root: tempfile::TempDir,
    previous: [(&'static str, Option<OsString>); 2],
}

impl EnforceEnv {
    fn new() -> Self {
        let lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
        let live = crate::config_live_reload::test_live_guard(lock);
        let root = tempfile::tempdir().unwrap();
        let keys = ["ADK_INTAKE_ROUTING_MODE", "AGENTDESK_ROOT_DIR"];
        let previous = keys.map(|key| (key, std::env::var_os(key)));
        // SAFETY: the crate-wide env lock is held until Drop restores both variables.
        unsafe {
            std::env::set_var(keys[0], "enforce");
            std::env::set_var(keys[1], root.path());
        }
        // Routing needs a known-empty owner allowlist independent of other tests.
        let mut config = crate::config::Config::default();
        config.data.dir = root.path().join("data");
        crate::config_live_reload::install(config);
        Self {
            _live: live,
            _root: root,
            previous,
        }
    }
}

impl Drop for EnforceEnv {
    fn drop(&mut self) {
        for (key, value) in self.previous.iter_mut() {
            // SAFETY: this guard still holds the crate-wide env lock.
            unsafe {
                match value.take() {
                    Some(value) => std::env::set_var(key, value),
                    None => std::env::remove_var(key),
                }
            }
        }
    }
}

fn submission(channel: u64, message: u64) -> IntakeSubmission {
    let message_id = MessageId::new(message);
    IntakeSubmission {
        provider: ProviderKind::Claude,
        request: IntakeRequest {
            intake_outbox_id: None,
            channel_id: ChannelId::new(channel),
            user_msg_id: message_id,
            source_message_ids: Vec::new(),
            busy_followup_retry_user_msg_id: message_id,
            request_owner: UserId::new(4386),
            request_owner_name: "home-order".into(),
            user_text: "hello".into(),
            reply_to_user_message: false,
            defer_watcher_resume: false,
            wait_for_completion: false,
            merge_consecutive: false,
            reply_context: None,
            has_reply_boundary: false,
            dm_hint: Some(false),
            turn_kind: TurnKind::Foreground,
            preserve_on_cancel: false,
        },
        origin: IntakeOrigin::LiveMessage,
        preserve_on_cancel: false,
        has_nonportable_uploads: false,
        attachments: Vec::new(),
        preloaded_uploads: Vec::new(),
        voice_announcement: None,
    }
}

/// Seeds ordinary foreign routing through a configured agent preference.
async fn seed(pool: &sqlx::PgPool) {
    for channel in [DELEGATED, OTHER] {
        sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ($1, 'Test', 'claude', $2)")
            .bind(format!("agent-{channel}"))
            .bind(channel.to_string())
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO worker_nodes (instance_id, status, role, effective_role, labels, capabilities,
         last_heartbeat_at, started_at, updated_at)
         VALUES ($1, 'online', 'worker', 'worker', '[]', $2, NOW(), NOW(), NOW())",
    )
    .bind(MINI)
    .bind(serde_json::json!({"intake_worker": {"enabled": true, "providers": ["claude"]}}))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("UPDATE agents SET preferred_intake_node_labels='[\"foreign-source\"]'::JSONB WHERE discord_channel_id=$1")
        .bind(DELEGATED.to_string()).execute(pool).await.unwrap();
    sqlx::query(
        "UPDATE worker_nodes SET labels='[\"foreign-source\"]'::JSONB WHERE instance_id=$1",
    )
    .bind(MINI)
    .execute(pool)
    .await
    .unwrap();
}

mod source_forwarding_tests {
    use super::*;
    // Exercises foreign admission and source fences through the actual delivery INSERT.

    async fn p2b_forward_sources(
        shared: &Arc<crate::services::discord::SharedData>,
        message: u64,
        sources: &[u64],
    ) -> i64 {
        let http = Arc::new(serenity::Http::new("Bot p2b-source-test"));
        let deps = IntakeDeps {
            http: &http,
            cache: None,
            ctx_for_chained_dispatch: None,
            shared,
            token: "Bot p2b-source-test",
        };
        let mut input = submission(DELEGATED, message);
        input.request.source_message_ids = sources.iter().copied().map(MessageId::new).collect();
        input.request.user_text = format!("merged source body {message}");
        input.request.reply_context = Some("original reply reference".into());
        match admit_text_intake(&deps, &input).await {
            IntakeAdmission::Forwarded {
                target_instance_id,
                outbox_id,
            } => {
                assert_eq!(target_instance_id, MINI);
                outbox_id
            }
            other => panic!("actual admission must forward to the foreign worker: {other:?}"),
        }
    }

    pub(super) async fn p2b_source_claim(
        pool: &sqlx::PgPool,
    ) -> Option<crate::db::intake_outbox::IntakeOutboxRow> {
        crate::db::intake_outbox::claim_pending_for_target_except(
            pool,
            MINI,
            "claude",
            "p2b-source-owner",
            &[],
        )
        .await
        .expect("actual worker claim must answer")
    }

    pub(super) async fn p2b_finish_source_claim(pool: &sqlx::PgPool, row: i64) {
        use crate::db::intake_outbox::{mark_accepted, mark_done, mark_spawned};
        assert!(mark_accepted(pool, row, "p2b-source-owner").await.unwrap());
        assert!(mark_spawned(pool, row, "p2b-source-owner").await.unwrap());
        assert!(mark_done(pool, row, "p2b-source-owner").await.unwrap());
    }

    #[tokio::test(flavor = "current_thread")]
    async fn b_active_foreign_delivery_carries_every_source_to_the_actual_claim_fence_pg() {
        use super::super::replay_admission::{
            PreparedEffectProjection, ReplayAdmissionContext, ReplayPreflight, discord_input,
            input_hash, preflight_source, prepare_effect_start, test_activation,
        };
        use crate::services::discord::replay_policy::permit::EffectTarget;

        let _env = EnforceEnv::new();
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
        seed(&pool).await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let _active = test_activation::activate(DELEGATED);
        let ctx =
            ReplayAdmissionContext::production(&pool, "p2b-source-leader", "source-incarnation")
                .for_scope(
                    &DELEGATED.to_string(),
                    "claude",
                    "legacy-prompt",
                    "owned_direct",
                    false,
                );

        for (offset, held_index) in [(0_u64, 0_usize), (10, 1)] {
            let first = DELEGATED * 100 + 1 + offset;
            let representative = first + 1;
            let sources = [first, representative];
            let row = p2b_forward_sources(&shared, representative, &sources).await;
            type ForwardedDeliveryRow = (
                Option<Vec<String>>,
                Option<String>,
                String,
                Option<String>,
                i32,
                bool,
            );
            let stored: ForwardedDeliveryRow = sqlx::query_as(
                "SELECT replay_source_message_ids, replay_request_key, user_text, reply_context,
                        attempt_no, replay_only FROM intake_outbox WHERE id=$1",
            )
            .bind(row)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                stored.0,
                Some(sources.iter().map(u64::to_string).collect()),
                "actual foreign INSERT must retain the absorbed source, not only its representative",
            );
            assert_eq!(
                stored.1, None,
                "delivery rows cannot occupy the canonical receipt's unique key"
            );
            assert_eq!(stored.2, format!("merged source body {representative}"));
            let context = crate::services::discord::router::response_format::retry_prompt_context::RetryPromptContext::decode(stored.3.as_deref()).unwrap();
            assert_eq!(context.raw.as_deref(), Some("original reply reference"));
            assert_eq!(context.selected, Default::default());
            assert!(context.may_select());
            assert_eq!((stored.4, stored.5), (1, false));

            let held_source = sources[held_index];
            let canonical = discord_input(
                "claude",
                DELEGATED,
                held_source,
                [held_source],
                "held source",
            );
            let candidate = match preflight_source(&ctx, &canonical).await {
                ReplayPreflight::Admit(candidate) => candidate,
                other => panic!("fresh canonical source must admit: {other:?}"),
            };
            let permit = prepare_effect_start(
                &ctx,
                &canonical,
                candidate,
                PreparedEffectProjection {
                    target: EffectTarget::ProviderStart {
                        provider: "claude".into(),
                        channel: DELEGATED.to_string(),
                    },
                    input_hash: input_hash("held source"),
                    binding: serde_json::json!({"source_test": held_source}),
                    session_key: None,
                },
                None,
            )
            .await
            .expect("canonical receipt can begin beside its delivery row without a key collision");
            let receipt = permit.attempt().receipt_id();
            assert_ne!(receipt, row);
            let canonical_key: String =
                sqlx::query_scalar("SELECT replay_request_key FROM intake_outbox WHERE id=$1")
                    .bind(receipt)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(canonical_key, canonical.request_key);
            drop(permit);

            assert!(
                p2b_source_claim(&pool).await.is_none(),
                "a hold on source index {held_index} must refuse the actual normal-delivery claim",
            );
            let untouched: (String, Option<String>, bool, bool, i32, i32, Option<Vec<String>>) = sqlx::query_as(
                "SELECT status::TEXT, claim_owner, accepted_at IS NULL, spawned_at IS NULL,
                        attempt_no, retry_count, replay_source_message_ids FROM intake_outbox WHERE id=$1",
            )
            .bind(row)
            .fetch_one(&pool)
            .await
            .unwrap();
            assert_eq!(
                untouched,
                (
                    "pending".into(),
                    None,
                    true,
                    true,
                    1,
                    0,
                    Some(sources.iter().map(u64::to_string).collect())
                ),
                "no executor admission, terminal settlement or retry is created by the denied claim",
            );
            let children: i64 =
                sqlx::query_scalar("SELECT count(*) FROM intake_outbox WHERE parent_outbox_id=$1")
                    .bind(row)
                    .fetch_one(&pool)
                    .await
                    .unwrap();
            assert_eq!(children, 0);

            sqlx::query(
                "UPDATE intake_outbox SET replay_disposition='classified_normal' WHERE id=$1",
            )
            .bind(receipt)
            .execute(&pool)
            .await
            .unwrap();
            let claimed = p2b_source_claim(&pool)
                .await
                .expect("ordinary classification reopens the same row");
            assert_eq!(
                claimed.id, row,
                "the negative claim has a real positive counterpart"
            );
            p2b_finish_source_claim(&pool, row).await;
        }
        pool.close().await;
        fixture.drop().await;
    }

    #[tokio::test(flavor = "current_thread")]
    async fn b_dormant_forwarding_and_old_null_source_delivery_keep_the_legacy_claim_path_pg() {
        use super::super::replay_admission::{
            PreparedEffectProjection, ReplayAdmissionContext, ReplayPreflight, discord_input,
            input_hash, preflight_source, prepare_effect_start,
        };
        use crate::services::discord::replay_policy::permit::EffectTarget;

        let _env = EnforceEnv::new();
        let fixture = TestPostgresDb::create().await;
        let pool = fixture.connect_and_migrate().await;
        crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
        seed(&pool).await;
        let shared =
            crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
        let first = DELEGATED * 100 + 41;
        let representative = first + 1;
        let row = p2b_forward_sources(&shared, representative, &[first, representative]).await;
        let stored: (Option<Vec<String>>, Option<String>) = sqlx::query_as(
            "SELECT replay_source_message_ids, replay_request_key FROM intake_outbox WHERE id=$1",
        )
        .bind(row)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            stored,
            (None, None),
            "no activation supplier preserves the pre-feature delivery row"
        );
        let receipt_rows: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM intake_outbox WHERE channel_id=$1 AND replay_disposition IS NOT NULL",
        )
        .bind(DELEGATED.to_string())
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(
            receipt_rows, 0,
            "dormant admission writes no replay authority"
        );
        assert_eq!(
            p2b_source_claim(&pool)
                .await
                .expect("legacy row remains claimable")
                .id,
            row
        );
        assert!(
            crate::db::intake_outbox::return_claimed_to_pending(&pool, row, "p2b-source-owner")
                .await
                .unwrap()
        );

        let canonical = discord_input(
            "claude",
            DELEGATED,
            representative,
            [representative],
            "old representative",
        );
        let ctx = ReplayAdmissionContext {
            producers: super::super::replay_admission::ProducerMode::Active,
            ..ReplayAdmissionContext::production(&pool, "p2b-old-source-leader", "old-incarnation")
        };
        let candidate = match preflight_source(&ctx, &canonical).await {
            ReplayPreflight::Admit(candidate) => candidate,
            other => panic!("old representative is a fresh canonical source: {other:?}"),
        };
        let permit = prepare_effect_start(
            &ctx,
            &canonical,
            candidate,
            PreparedEffectProjection {
                target: EffectTarget::ProviderStart {
                    provider: "claude".into(),
                    channel: DELEGATED.to_string(),
                },
                input_hash: input_hash("old representative"),
                binding: serde_json::json!({"old_source_test": representative}),
                session_key: None,
            },
            None,
        )
        .await
        .unwrap();
        let receipt = permit.attempt().receipt_id();
        drop(permit);
        assert!(
            p2b_source_claim(&pool).await.is_none(),
            "NULL source rows still fence their representative"
        );
        sqlx::query("UPDATE intake_outbox SET replay_disposition='classified_normal' WHERE id=$1")
            .bind(receipt)
            .execute(&pool)
            .await
            .unwrap();
        assert_eq!(
            p2b_source_claim(&pool)
                .await
                .expect("ordinary old row resumes")
                .id,
            row
        );
        p2b_finish_source_claim(&pool, row).await;
        pool.close().await;
        fixture.drop().await;
    }
}

#[tokio::test(flavor = "current_thread")]
async fn dormant_owner_admission_primitive_preserves_full_sources_without_receipt_key_pg() {
    use super::replay_admission::discord_input;
    use crate::db::intake_outbox::InsertPendingPayload;
    use crate::db::replay_disposition::write;
    use crate::services::cluster::intake_router_hook::owner_record::{
        AcquireOutcome, AdmissionKind, AdmissionOutcome, OwnerIdentity, acquire_owner_in_tx,
        idempotency_key, insert_admission_savepoint,
    };

    let _env = EnforceEnv::new();
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    crate::db::replay_disposition::tests::apply_test_mutant(&pool).await;
    seed(&pool).await;
    let representative = OTHER * 100 + 62;
    let sources = vec![(representative - 1).to_string(), representative.to_string()];
    let payload = InsertPendingPayload {
        target_instance_id: MINI.into(),
        forwarded_by_instance_id: GW.into(),
        required_labels: serde_json::json!([]),
        execution_requirements: serde_json::json!({}),
        attachment_refs: serde_json::json!([]),
        channel_id: OTHER.to_string(),
        user_msg_id: representative.to_string(),
        request_owner_id: "4386".into(),
        request_owner_name: Some("home-order".into()),
        user_text: "owner primitive full body".into(),
        reply_context: Some("owner primitive reply".into()),
        has_reply_boundary: true,
        dm_hint: Some(false),
        turn_kind: "foreground".into(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: format!("agent-{OTHER}"),
        provider: "claude".into(),
        replay_source_message_ids: Some(sources.clone()),
    };
    let identity = OwnerIdentity::new("claude", &OTHER.to_string());
    let key = idempotency_key("claude", &OTHER.to_string(), &representative.to_string(), 1);
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(identity.advisory_key())
        .execute(&mut *tx)
        .await
        .unwrap();
    let generation = match acquire_owner_in_tx(&mut tx, &identity, MINI, |_| true)
        .await
        .unwrap()
    {
        AcquireOutcome::AcquiredLocal { generation } => generation,
        other => panic!("the fresh primitive identity must acquire its owner: {other:?}"),
    };
    let id = match insert_admission_savepoint(
        &mut tx,
        &payload,
        AdmissionKind::Forwarded,
        MINI,
        generation,
        MINI,
        &key,
        1,
    )
    .await
    .unwrap()
    {
        AdmissionOutcome::Inserted { outbox_id } => outbox_id,
        other => panic!("the actual owner SQL must INSERT the normal delivery: {other:?}"),
    };
    tx.commit().await.unwrap();
    type OwnerDeliveryRow = (
        Option<Vec<String>>,
        Option<String>,
        bool,
        String,
        Option<String>,
        Option<i64>,
        String,
        Option<String>,
    );
    let row: OwnerDeliveryRow = sqlx::query_as(
        "SELECT replay_source_message_ids, replay_request_key, replay_only, status::TEXT,
                owner_instance_id, owner_generation, admission_kind, idempotency_key
           FROM intake_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        row,
        (
            Some(sources.clone()),
            None,
            false,
            "pending".into(),
            Some(MINI.into()),
            Some(generation),
            "forwarded".into(),
            Some(key)
        ),
        "dormant owner INSERT copies full source metadata without claiming the canonical receipt key"
    );
    let content: (String, Option<String>, serde_json::Value) = sqlx::query_as(
        "SELECT user_text, reply_context, attachment_refs FROM intake_outbox WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        content,
        (
            payload.user_text.clone(),
            payload.reply_context.clone(),
            payload.attachment_refs.clone()
        )
    );

    let canonical = discord_input(
        "claude",
        OTHER,
        representative,
        sources.iter().map(|source| source.parse::<u64>().unwrap()),
        &payload.user_text,
    );
    let receipt = write::register_or_reuse(&pool, &canonical, "owner-primitive-canonical")
        .await
        .unwrap();
    assert_ne!(
        receipt.id, id,
        "the separate canonical receipt is not a duplicate of the normal owner delivery"
    );
    let stored_key: Option<String> =
        sqlx::query_scalar("SELECT replay_request_key FROM intake_outbox WHERE id=$1")
            .bind(id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(stored_key, None);
    let canonical_key: String =
        sqlx::query_scalar("SELECT replay_request_key FROM intake_outbox WHERE id=$1")
            .bind(receipt.id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(canonical_key, canonical.request_key);
    let claimed = source_forwarding_tests::p2b_source_claim(&pool)
        .await
        .expect("a registered-not-started receipt permits the normal delivery claim");
    assert_eq!(claimed.id, id);
    source_forwarding_tests::p2b_finish_source_claim(&pool, id).await;
    pool.close().await;
    fixture.drop().await;
}
