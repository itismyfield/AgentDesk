//! A delegated channel's intake reaches the router's home decision before the gateway's placement
//! hold; every other channel is placed exactly as before, ahead of any Postgres read.
use std::ffi::OsString;
use std::sync::Arc;

use poise::serenity_prelude as serenity;
use serenity::{ChannelId, MessageId, UserId};

use super::{IntakeAdmission, IntakeOrigin, IntakeSubmission, admit_text_intake};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::o_channel_homes::{HomeWrite, adopt, delegate, finish_release};
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::cluster::channel_home::{self, HomeGate};
use crate::services::cluster::intake_router_hook::IntakeBlockedReason;
use crate::services::discord::router::TurnKind;
use crate::services::discord::router::message_handler::{IntakeDeps, IntakeRequest};
use crate::services::provider::ProviderKind;
use crate::services::tui_o::cutover::{intake_route::test_probe, test_override};

const DELEGATED: u64 = 4_380_601;
const SELECTED: u64 = 4_380_602;
const OTHER: u64 = 4_380_603;
const GW: &str = "gw-4386";
const MINI: &str = "mini-4386";

/// Enforce routing over a fresh root, holding the crate-wide env lock.
struct EnforceEnv {
    _lock: std::sync::MutexGuard<'static, ()>,
    _root: tempfile::TempDir,
    previous: [(&'static str, Option<OsString>); 2],
}

impl EnforceEnv {
    fn new() -> Self {
        let lock = crate::config::shared_test_env_lock()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let root = tempfile::tempdir().unwrap();
        let keys = ["ADK_INTAKE_ROUTING_MODE", "AGENTDESK_ROOT_DIR"];
        let previous = keys.map(|key| (key, std::env::var_os(key)));
        // SAFETY: the crate-wide env lock is held until Drop restores both variables.
        unsafe {
            std::env::set_var(keys[0], "enforce");
            std::env::set_var(keys[1], root.path());
        }
        Self {
            _lock: lock,
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

async fn admit(
    shared: &Arc<crate::services::discord::SharedData>,
    channel: u64,
) -> IntakeAdmission {
    let http = Arc::new(serenity::Http::new("Bot home-order-test"));
    let deps = IntakeDeps {
        http: &http,
        cache: None,
        ctx_for_chained_dispatch: None,
        shared,
        token: "Bot home-order-test",
    };
    admit_text_intake(&deps, &submission(channel, channel + 10)).await
}

/// The placement hold's refusal, or a failure to say why this was not one.
fn placement_hold(admission: IntakeAdmission) -> String {
    match admission {
        IntakeAdmission::Blocked {
            reason: IntakeBlockedReason::RoutingDependencyFailed { detail },
        } => detail,
        other => panic!("expected the placement hold, got {other:?}"),
    }
}

async fn seed(pool: &sqlx::PgPool) {
    for channel in [DELEGATED, SELECTED, OTHER] {
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
    let channel = DELEGATED.to_string();
    let applied = |write| match write {
        Ok(HomeWrite::Applied(home)) => home,
        other => panic!("home write: {other:?}"),
    };
    let home = applied(delegate(pool, &channel, "claude", GW, MINI).await);
    let home = applied(finish_release(pool, &channel, GW, home.epoch).await);
    applied(adopt(pool, &channel, MINI, home.epoch).await);
}

// Unregistered, beside another channel's gate, or without Postgres, placement still holds first,
// before any home read; a registered gate with Postgres reaches the holder through the home row.
#[tokio::test(flavor = "current_thread")]
async fn only_a_registered_delegated_channel_skips_the_placement_hold_and_reaches_its_holder_pg() {
    let _env = EnforceEnv::new();
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    seed(&pool).await;
    let shared =
        crate::services::discord::make_shared_data_for_tests_with_storage(Some(pool.clone()));
    let no_pool = crate::services::discord::make_shared_data_for_tests();
    let _selected = test_override::force_channels(&[(SELECTED, ClaudeTui)]);

    // Any home read would fail now, so a hold below is decided before Postgres is read.
    sqlx::query("ALTER TABLE o_channel_homes RENAME TO o_channel_homes_away")
        .execute(&pool)
        .await
        .unwrap();
    assert!(!channel_home::any_registered(), "the registry starts unset");
    let _not_ready = test_probe::answers(&[false]);
    let held = placement_hold(admit(&shared, SELECTED).await);
    assert!(held.contains("gateway"), "registry unset: {held}");
    let other = Arc::new(HomeGate::new(&OTHER.to_string(), GW));
    channel_home::register(Arc::clone(&other));
    let _not_ready = test_probe::answers(&[false]);
    let held = placement_hold(admit(&shared, SELECTED).await);
    assert!(held.contains("gateway"), "another channel's gate: {held}");
    let gate = Arc::new(HomeGate::new(&DELEGATED.to_string(), GW));
    channel_home::register(Arc::clone(&gate));
    let held = placement_hold(admit(&no_pool, DELEGATED).await);
    assert!(held.contains("delegated"), "without Postgres: {held}");
    sqlx::query("ALTER TABLE o_channel_homes_away RENAME TO o_channel_homes")
        .execute(&pool)
        .await
        .unwrap();

    let forwarded = admit(&shared, DELEGATED).await;
    assert!(
        matches!(&forwarded, IntakeAdmission::Forwarded { target_instance_id, .. }
            if target_instance_id == MINI),
        "{forwarded:?}"
    );
    let stamped: Option<i64> = sqlx::query_scalar(
        "SELECT home_epoch FROM intake_outbox WHERE channel_id = $1 AND target_instance_id = $2",
    )
    .bind(DELEGATED.to_string())
    .bind(MINI)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(stamped.is_some(), "routed at the home's epoch");

    channel_home::unregister(&DELEGATED.to_string());
    channel_home::unregister(&OTHER.to_string());
    pool.close().await;
    fixture.drop().await;
}
