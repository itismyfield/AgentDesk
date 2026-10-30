//! A registered channel the runtime has not seen yet: its first catch-up input reaches the
//! queued-turn promote with no channel session, and the registered name or its row decides it.

use std::time::Duration;

use tokio::sync::broadcast::Receiver;

use super::{CHANNEL_ID, RelayE2eHarness};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::services::discord::host_defer_gate::tests::{Nameless, postgres};
use crate::services::discord::turn_completion_events::TurnCompletionEvent;
use crate::services::discord::{ProviderKind, inflight, kickoff_idle_queue_channel, router};

pub(super) const FALLBACK: &str = "p4c1f-registered";

/// What the runtime finds for the registered name besides the queued input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Trace {
    Nothing,
    HostedRow,
    HostedRowUnderKeyOnly,
    LegacyRowHerdrMarker,
    HerdrInflight,
    FailedRead,
    /// The hosted row of this node's Claude execution, with its input held back.
    ReturningRowHeld,
    /// That execution's row on a channel registered by id alone.
    ReturningIdOnly,
    /// That execution retired with its move back unfinished, on a channel registered by id alone.
    RetiredIdOnly,
}

pub(super) fn recent_message_id() -> u64 {
    const DISCORD_EPOCH_MS: i64 = 1_420_070_400_000;
    let discord_ms = chrono::Utc::now().timestamp_millis() - 30_000 - DISCORD_EPOCH_MS;
    (u64::try_from(discord_ms).expect("after Discord epoch") << 22) | 1
}

pub(super) fn register_channel(harness: &RelayE2eHarness) {
    register_channel_as(harness, Some(FALLBACK));
}

/// The channel registered by its id, under `name` when one is given.
fn register_channel_as(harness: &RelayE2eHarness, name: Option<&str>) {
    let config = harness.root.path().join("config").join("agentdesk.yaml");
    std::fs::create_dir_all(config.parent().unwrap()).unwrap();
    let workspace = harness.root.path().display();
    let name = name.map_or(String::new(), |name| format!("        name: \"{name}\"\n"));
    let yaml = format!(
        "server:\n  port: 8791\nagents:\n  - id: p4c1f\n    name: \"P4c1f\"\n    provider: claude\n    \
         channels:\n      claude:\n        id: \"{CHANNEL_ID}\"\n{name}        \
         workspace: \"{workspace}\"\n"
    );
    std::fs::write(&config, yaml).unwrap();
    let validated = crate::config::load_from_path(&config).unwrap();
    crate::config_live_reload::install(validated);
}

#[tokio::test(flavor = "current_thread")]
async fn registered_channel_snapshot_is_restored_after_harness_drop() {
    const CHILD: &str = "ADK_TEST_REGISTERED_SNAPSHOT_RESTORE";
    if std::env::var_os(CHILD).is_none() {
        let path = concat!(
            module_path!(),
            "::registered_channel_snapshot_is_restored_after_harness_drop"
        );
        let name = path.split_once("::").unwrap().1;
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", name, "--nocapture", "--test-threads=1"])
            .env(CHILD, "1")
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success()
                && stdout.lines().any(|line| line
                    .starts_with("test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; ")),
            "{stdout}\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    assert!(crate::config::snapshot().is_none());
    for installed in [false, true] {
        if installed {
            let mut config = crate::config::Config::default();
            config.server.port = 39134;
            crate::config_live_reload::install(config);
        }
        let previous = crate::config::snapshot();
        let harness = RelayE2eHarness::start().await;
        register_channel(&harness);
        let registered = crate::config::snapshot().unwrap();
        assert_eq!(registered.agents[0].id, "p4c1f");
        drop(harness);
        let restored = crate::config::snapshot();
        match (previous, restored) {
            (None, None) => {}
            (Some(previous), Some(restored)) => {
                assert!(std::sync::Arc::ptr_eq(&previous, &restored));
            }
            _ => panic!("the harness must restore the previous snapshot presence"),
        }
    }
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
        Trace::ReturningRowHeld => {
            let case = Nameless::Hosted;
            case.seed(pool, own, other, CHANNEL_ID, &tmux_name).await;
            let hold = crate::services::claude::herdr_turn::hold_path("n1").unwrap();
            std::fs::create_dir_all(hold.parent().unwrap()).unwrap();
            std::fs::write(hold, "a draft").unwrap();
        }
        Trace::ReturningIdOnly | Trace::RetiredIdOnly => {
            use crate::db::dispatched_sessions::hosted_execution::HostedState;
            use crate::db::dispatched_sessions::hosted_execution::tests::{owner, record, wire};
            let mut row_owner = owner(&CHANNEL_ID.to_string());
            row_owner.discord_token_hash = own.to_string();
            row_owner.logical_key = tmux_name.clone();
            let state = match trace {
                Trace::ReturningIdOnly => HostedState::Bound,
                _ => HostedState::Retired,
            };
            let raw = wire(&record(&row_owner, "n1", state));
            let key = crate::services::discord::adk_session::build_namespaced_session_key;
            let key = key(own, &ProviderKind::Claude, &tmux_name);
            let seed_row = crate::services::discord::host_key_derivation::tests::seed_row;
            seed_row(pool, "claude", Some(own), &key, CHANNEL_ID, Some(raw)).await;
            // The Herdr launch left the marker naming Herdr; an unfinished move back keeps it.
            let marker = crate::services::tmux_common::session_temp_path(&tmux_name, "host_kind");
            std::fs::create_dir_all(std::path::Path::new(&marker).parent().unwrap()).unwrap();
            std::fs::write(marker, "herdr").unwrap();
        }
        Trace::Nothing => {}
    }
}

/// The database drops before the harness releases the env lock it was opened under.
struct Run {
    started: bool,
    queued: usize,
    completions: Receiver<TurnCompletionEvent>,
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
    match trace {
        Trace::ReturningIdOnly | Trace::RetiredIdOnly => register_channel_as(&harness, None),
        _ => register_channel(&harness),
    }
    harness.answer_placeholders_immediately();
    seed(trace, &pool, &harness.shared.token_hash).await;
    let id = recent_message_id();
    harness.seed_channel_history(&[(id, "first input after the bot was away", false)]);
    let completions = harness.subscribe_completions();
    harness.run_catch_up().await;
    assert!(
        harness.shared.core.lock().await.sessions.is_empty(),
        "no channel session"
    );
    let started = kick_off(&harness).await;
    let queued = harness.mailbox().await.intervention_queue.len();
    Run {
        started,
        queued,
        completions,
        db,
        harness,
    }
}

/// The production idle kickoff for the channel; whether it started a turn.
pub(super) async fn kick_off(harness: &RelayE2eHarness) -> bool {
    let deps = router::IntakeDeps {
        http: &harness.ctx.http,
        cache: Some(&harness.ctx.cache),
        ctx_for_chained_dispatch: Some(&harness.ctx),
        shared: &harness.shared,
        token: &harness.data.token,
    };
    let channel = harness.channel_id;
    let outcome = kickoff_idle_queue_channel(&deps, &ProviderKind::Claude, channel).await;
    outcome.started
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_registered_new_channel_starts_its_first_catch_up_input_once_pg() {
    let mut run = first_catch_up_input(Trace::Nothing).await;
    assert!(
        run.started,
        "the registered name with no row and no trace starts the turn"
    );
    assert_eq!(run.queued, 0, "the input left the queue");
    let completed = tokio::time::timeout(Duration::from_secs(10), run.completions.recv()).await;
    completed
        .expect("the first turn completes")
        .expect("completion bus open");
    let harness = &run.harness;
    // The same history caught up again must not start the answered input a second time.
    harness.run_catch_up().await;
    assert!(!kick_off(harness).await, "a second kickoff starts nothing");
    let mailbox = harness.mailbox().await;
    assert!(mailbox.intervention_queue.is_empty(), "nothing re-queued");
    assert_eq!(
        mailbox.pending_user_dispatch, None,
        "no dispatch left pending"
    );
    assert_eq!(harness.provider_starts(), 1, "the provider started once");
    assert_eq!(harness.placeholder_posts(), 1, "one turn, one placeholder");
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

// A channel whose Herdr mapping was removed starts its first queued input, whose turn moves this
// node's Claude execution back to tmux; a held input there stops it before any exit or launch.
#[test]
fn the_first_queued_input_of_a_channel_that_left_herdr_starts_its_return_pg() {
    on_this_node_unmapped(first_queued_input_returns());
}

/// Runs `scenario` with every runtime thread on this node and no channel mapped to Herdr; hosts
/// are forced per thread.
fn on_this_node_unmapped(scenario: impl std::future::Future<Output = ()>) {
    let this_node = || {
        std::mem::forget(crate::config::session_hosts::force_for_test(
            Some("test-node"),
            &[],
        ))
    };
    this_node();
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .enable_all()
        .on_thread_start(this_node)
        .build()
        .unwrap()
        .block_on(scenario);
}

// An id-only channel restarted with no session and its Herdr mapping removed: its first queued
// input finds the row by channel id, moves Claude back (a retired row without an exit), runs once.
#[test]
fn an_id_only_channel_that_left_herdr_returns_on_its_first_queued_input_pg() {
    on_this_node_unmapped(async {
        id_only_input_returns(Trace::ReturningIdOnly).await;
        id_only_input_returns(Trace::RetiredIdOnly).await;
    });
}

async fn id_only_input_returns(trace: Trace) {
    let logical = ProviderKind::Claude.build_tmux_session_name(FALLBACK);
    let stand_in = crate::services::claude::herdr_return::tests::StandIn::new(&logical);
    stand_in.herdr.lock().unwrap().alive = trace == Trace::ReturningIdOnly;
    let mut run = first_catch_up_input(trace).await;
    assert!(run.started, "{trace:?}: the queue starts the return turn");
    assert_eq!(run.queued, 0, "{trace:?}: the input left the queue");
    let completed = tokio::time::timeout(Duration::from_secs(10), run.completions.recv()).await;
    completed
        .expect("the turn ends")
        .expect("completion bus open");
    let exits = usize::from(trace == Trace::ReturningIdOnly);
    assert_eq!(stand_in.herdr.lock().unwrap().exits, exits, "{trace:?}");
    assert_eq!(stand_in.tmux.lock().unwrap().exits, 0, "{trace:?}");
    use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
    let pool = run.harness.shared.pg_pool.clone().unwrap();
    let raw: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id = $1")
            .bind(CHANNEL_ID.to_string())
            .fetch_one(&pool)
            .await
            .unwrap();
    let HostedRecord::Known(record) = HostedRecord::decode(raw.as_ref()) else {
        panic!("{raw:?}");
    };
    assert_eq!(record.state, HostedState::Retired, "{trace:?}");
    let tmux = crate::services::session_host::HostKind::Tmux;
    let marker = crate::services::tmux_common::host_marker::read_host_kind_marker(&logical);
    assert_eq!(
        marker,
        crate::services::tmux_common::host_marker::HostKindMarker::Known(tmux)
    );
    let harness = &run.harness;
    assert_eq!(harness.provider_starts(), 1, "{trace:?}: one tmux turn");
    let inputs = harness.provider_inputs();
    let first = "first input after the bot was away";
    assert!(inputs[0].contains(first), "{inputs:?}");
    // The harness holds the answer behind its O writer, so a later catch-up would read the input
    // as unanswered; with nothing new queued, another kickoff starts nothing and ends nothing.
    assert!(!kick_off(harness).await, "{trace:?}: the input starts once");
    assert_eq!(stand_in.herdr.lock().unwrap().exits, exits, "{trace:?}");
    assert_eq!(harness.provider_starts(), 1, "{trace:?}");
    run.close().await;
}

async fn first_queued_input_returns() {
    let mut run = first_catch_up_input(Trace::ReturningRowHeld).await;
    assert!(run.started, "the queue starts the return turn");
    assert_eq!(run.queued, 0, "the input left the queue");
    let completed = tokio::time::timeout(Duration::from_secs(10), run.completions.recv()).await;
    completed
        .expect("the turn ends")
        .expect("completion bus open");
    let harness = &run.harness;
    // The harness holds Discord output behind its O writer, so the turn keeps its answer here.
    let row =
        inflight::load_inflight_state(&ProviderKind::Claude, CHANNEL_ID).expect("the turn's row");
    assert!(
        row.full_response.contains("herdr return") && row.full_response.contains("unsubmitted"),
        "{}",
        row.full_response
    );
    assert_eq!(harness.provider_starts(), 0, "no tmux Claude started");
    let pool = harness.shared.pg_pool.as_ref().unwrap();
    let raw: Option<serde_json::Value> =
        sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id = $1")
            .bind(CHANNEL_ID.to_string())
            .fetch_one(pool)
            .await
            .unwrap();
    use crate::db::dispatched_sessions::hosted_execution::{HostedRecord, HostedState};
    let HostedRecord::Known(record) = HostedRecord::decode(raw.as_ref()) else {
        panic!("{raw:?}");
    };
    assert_eq!(
        record.state,
        HostedState::Bound,
        "the row is left as it was"
    );
    run.close().await;
}
