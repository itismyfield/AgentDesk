//! A home that moves after the worker's last check takes effect at accept: no turn starts there.
use super::*;
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::intake_outbox::{InsertPendingPayload, insert_pending};
use crate::db::o_channel_homes::{
    ChannelHome, HeldHome, HomeError, HomeState, HomeWrite, adopt, begin_reclaim, delegate,
    finish_release,
};
use crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui;
use crate::services::cluster::channel_home::{self as home_gate, HomeGate};
use crate::services::cluster::channel_home_drain::{
    Blocker, DrainPort, DrainStep, Owed, ResetRefused, drain_round,
};
use crate::services::tui_o::channel_policy::Adoption;
use crate::services::tui_o::cutover::{intake_route::test_probe, test_override};
use test_executor::Checkpoint;

const C: u64 = 4_380_501;
const GW: &str = "gw-4385";
const MINI: &str = "mini-4385";

fn applied(write: Result<HomeWrite<ChannelHome>, HomeError>) -> ChannelHome {
    match write.expect("home write") {
        HomeWrite::Applied(home) => home,
        HomeWrite::Stale => panic!("expected the home write to apply"),
    }
}

/// A holder whose turn, owed pieces and POSTs are all done, so only open intake can hold it.
struct Idle;

impl DrainPort for Idle {
    async fn turn_running(&self) -> Option<bool> {
        Some(false)
    }

    async fn owed(&self) -> Option<Owed> {
        Some(Owed::default())
    }

    async fn posts_in_flight(&self) -> Option<usize> {
        Some(0)
    }

    async fn reset_legacy_source(&self) -> Result<(), ResetRefused> {
        Ok(())
    }
}

// A reclaim lands and the holder's drain closes for good after its last check, before accept: no
// accept and no turn; the row stays pending, so the drain waits on it instead of leaving.
#[tokio::test(flavor = "current_thread")]
async fn a_home_moved_after_the_last_check_refuses_the_accept_and_keeps_the_row_open_pg() {
    let fixture = TestPostgresDb::create().await;
    let pool = fixture.connect_and_migrate().await;
    let channel = C.to_string();
    sqlx::query("INSERT INTO agents (id, name, provider, discord_channel_id) VALUES ($1, 'Test', 'claude', $2)")
        .bind(format!("agent-{C}"))
        .bind(&channel)
        .execute(&pool)
        .await
        .unwrap();
    let home = applied(delegate(&pool, &channel, "claude", GW, MINI).await);
    let home = applied(finish_release(&pool, &channel, GW, home.epoch).await);
    let epoch = applied(adopt(&pool, &channel, MINI, home.epoch).await).epoch;
    let payload = InsertPendingPayload {
        target_instance_id: MINI.into(),
        forwarded_by_instance_id: GW.into(),
        required_labels: serde_json::json!([]),
        execution_requirements: serde_json::json!({}),
        attachment_refs: serde_json::json!([]),
        channel_id: channel.clone(),
        user_msg_id: "1".into(),
        request_owner_id: "100".into(),
        request_owner_name: None,
        user_text: "hello".into(),
        reply_context: None,
        has_reply_boundary: false,
        dm_hint: None,
        turn_kind: "foreground".into(),
        merge_consecutive: false,
        reply_to_user_message: false,
        defer_watcher_resume: false,
        wait_for_completion: false,
        preserve_on_cancel: false,
        agent_id: format!("agent-{C}"),
        provider: "claude".into(),
        home_epoch: Some(epoch),
    };
    let row = insert_pending(&pool, &payload, 1, None).await.unwrap();
    let (_registry, shared) =
        crate::services::discord::health::owner_runtime_for_tests::registered("claude").await;
    let _standby = test_override::force_standby(&[(C, ClaudeTui)], GW, Adoption::Committed);
    let _ready = test_probe::answer_with(|_| true);
    let gate = Arc::new(HomeGate::new(&channel, MINI));
    home_gate::register(Arc::clone(&gate));
    let renewal = HeldHome::for_test(&channel, MINI, epoch, HomeState::Worker);
    gate.confirm(&renewal, tokio::time::Instant::now())
        .expect("the holder opens");

    let (moving_pool, moving_gate) = (pool.clone(), Arc::clone(&gate));
    let _hook = test_executor::hook(Box::new(move |seen| {
        let (pool, gate) = (moving_pool.clone(), Arc::clone(&moving_gate));
        Box::pin(async move {
            if seen == Checkpoint::PreAccept {
                let channel = C.to_string();
                applied(begin_reclaim(&pool, &channel, epoch, GW).await);
                gate.close_intake();
                gate.close();
            }
        })
    }));
    let recorder = test_executor::record();
    let outcome = run_intake_worker_tick(&pool, &shared, MINI, "claude", "o", &|| false).await;

    assert_eq!(outcome.unwrap(), TickOutcome::Held);
    assert_eq!(recorder.channels(), Vec::<u64>::new(), "no turn started");
    let stored: (String, Option<String>, bool, Option<i64>) = sqlx::query_as(
        "SELECT status::TEXT, claim_owner, accepted_at IS NULL, home_epoch
         FROM intake_outbox WHERE id = $1",
    )
    .bind(row)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored, ("pending".into(), None, true, Some(epoch)));
    let step = drain_round(&pool, &gate, &Idle).await;
    assert!(
        matches!(step, DrainStep::Waiting(Blocker::OpenIntake(1))),
        "{step:?}"
    );

    home_gate::unregister(&channel);
    pool.close().await;
    fixture.drop().await;
}
