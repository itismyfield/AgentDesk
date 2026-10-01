//! A registered channel the runtime has not seen yet: its first catch-up input reaches the
//! queued-turn promote with no channel session, and the registered name decides it.

use std::time::Duration;

use super::{CHANNEL_ID, RelayE2eHarness};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::host_defer_gate::tests::{Nameless, postgres};
use crate::services::discord::{ProviderKind, inflight, kickoff_idle_queue_channel, router};

const FALLBACK: &str = "p4c1f-registered";

/// What the runtime finds for the registered name besides the queued input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trace {
    Nothing,
    HostedRow,
    HostedRowUnderKeyOnly,
    LegacyRowHerdrMarker,
    HerdrInflight,
    FailedRead,
}

fn recent_message_id() -> u64 {
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let discord_ms = chrono::Utc::now().timestamp_millis() - 30_000 - DISCORD_EPOCH_MS;
    (u64::try_from(discord_ms).expect("after Discord epoch") << 22) | 1
}

fn register_channel(harness: &RelayE2eHarness) {
    let config = harness.root.path().join("config").join("agentdesk.yaml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let workspace = harness.root.path().display();
    let yaml = format!(
        "server:\n  port: 8791\nagents:\n  - id: p4c1f\n    name: \"P4c1f\"\n    provider: claude\n    \
         channels:\n      claude:\n        id: \"{CHANNEL_ID}\"\n        name: \"{FALLBACK}\"\n        \
         workspace: \"{workspace}\"\n"
    );
    std::fs::write(config, yaml).unwrap();
}

async fn seed(trace: Trace, pool: &sqlx::PgPool, own: &str) {
    let tmux_name = ProviderKind::Claude.build_tmux_session_name(FALLBACK);
    let other = "p4c1f-other-bot";
    match trace {
        Trace::HostedRow => {
            Nameless::Hosted
                .seed(pool, own, other, CHANNEL_ID, &tmux_name)
                .await
        }
        Trace::HostedRowUnderKeyOnly => {
            let case = Nameless::HostedKeyOnly;
            case.seed(pool, own, other, CHANNEL_ID, &tmux_name).await;
        }
        Trace::LegacyRowHerdrMarker => {
            let case = Nameless::LegacyHerdrMarker;
            case.seed(pool, own, other, CHANNEL_ID, &tmux_name).await;
        }
        Trace::HerdrInflight => {
            let row = inflight::InflightTurnState::new(
                ProviderKind::Claude,
                CHANNEL_ID,
                None,
                1,
                CHANNEL_ID + 1,
                CHANNEL_ID + 2,
                "a Herdr turn in flight".to_string(),
                None,
                None,
                None,
                None,
                0,
            );
            let mut wire = serde_json::to_value(&row).expect("inflight wire");
            wire["host_locator"] = serde_json::json!({
                "host_kind": "herdr", "host_session_id": "p4c1f-workspace", "pane": "p4c1f-pane"
            });
            let row = serde_json::from_value(wire).expect("inflight row with a Herdr locator");
            inflight::save_inflight_state_create_new(&row).expect("inflight row");
        }
        Trace::FailedRead => {
            // The sessions table goes unreadable; queue and turn storage still work.
            sqlx::query("ALTER TABLE sessions RENAME TO sessions_unreadable")
                .execute(pool)
                .await
                .expect("hide the sessions table");
        }
        Trace::Nothing => {}
    }
}

/// The database drops before the harness releases the env lock it was opened under.
struct Run {
    started: bool,
    queued: usize,
    db: TestPostgresDb,
    harness: RelayE2eHarness,
}

impl Run {
    async fn close(self) {
        self.harness.shared.pg_pool.as_ref().unwrap().close().await;
        self.db.drop().await;
    }
}

/// Runs a production catch-up over one unanswered input, then the production kickoff.
async fn first_catch_up_input(trace: Trace) -> Run {
    let mut db = None;
    let harness = RelayE2eHarness::start_unbound_on(async {
        let (fixture, pool) = postgres().await;
        db = Some(fixture);
        pool
    })
    .await;
    let db = db.expect("a database under the harness lock");
    let pool = harness
        .shared
        .pg_pool
        .clone()
        .expect("a runtime on PostgreSQL");
    register_channel(&harness);
    harness.answer_placeholders_immediately();
    seed(trace, &pool, &harness.shared.token_hash).await;
    let id = recent_message_id();
    harness.seed_channel_history(&[(id, "first input after the bot was away", false)]);
    harness.run_catch_up().await;
    let shared = &harness.shared;
    assert!(
        shared.core.lock().await.sessions.is_empty(),
        "no channel session"
    );
    let deps = router::IntakeDeps {
        http: &harness.ctx.http,
        cache: Some(&harness.ctx.cache),
        ctx_for_chained_dispatch: Some(&harness.ctx),
        shared,
        token: &harness.data.token,
    };
    let channel = harness.channel_id;
    let outcome = kickoff_idle_queue_channel(&deps, &ProviderKind::Claude, channel).await;
    let queued = harness.mailbox().await.intervention_queue.len();
    Run {
        started: outcome.started,
        queued,
        db,
        harness,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_registered_new_channel_starts_its_first_catch_up_input_once_pg() {
    let run = first_catch_up_input(Trace::Nothing).await;
    assert!(
        run.started,
        "the registered name with no row and no trace starts the turn"
    );
    assert_eq!(run.queued, 0, "the input left the queue");
    let harness = &run.harness;
    let posted = harness.wait_for_placeholder_posts(1, Duration::from_secs(5));
    assert!(posted.await, "the started turn posts its placeholder");
    assert_eq!(harness.placeholder_posts(), 1, "one turn, started once");
    run.close().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn host_evidence_on_the_registered_name_holds_the_first_catch_up_input_pg() {
    for trace in [
        Trace::HostedRow,
        Trace::HostedRowUnderKeyOnly,
        Trace::LegacyRowHerdrMarker,
        Trace::HerdrInflight,
        Trace::FailedRead,
    ] {
        let run = first_catch_up_input(trace).await;
        assert!(!run.started, "{trace:?}");
        assert_eq!(run.queued, 1, "{trace:?} keeps the input queued");
        assert_eq!(run.harness.placeholder_posts(), 0, "{trace:?}");
        run.close().await;
    }
}
