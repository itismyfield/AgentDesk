//! X1 on real Herdr 0.9.3: `agentdesk herdr terminate` closes the last pane of the target's
//! workspace and retires exactly its execution, while O's writer, down during the termination,
//! afterwards delivers the target's sealed tail once and the service wrote none of O's state.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use serde_json::Value;

use super::{
    CHANNEL_ID, OHost, RESTART_OUTPUT, TAIL, append, assistant, begin_era, bind_source, cursor_of,
    o_state, posted_in_ledger, root,
};
use crate::db::auto_queue::test_support::TestPostgresDb;
use crate::db::dispatched_sessions::hosted_execution::HostedState;
use crate::services::agent_protocol::RuntimeHandoffKind;
use crate::services::discord::herdr_terminate::route_tests::capture;
use crate::services::discord::herdr_terminate::test_turn;
use crate::services::session_host::herdr_terminate_real_tests::{
    ROUTE_REQUESTS, Real, Scratch, Witnessing, alive, boot, cli_terminate, control, field, launch,
    print_panes, print_results, serve_api, stored, the_close,
};
use crate::services::tui_o::shadow::binding_reader::source_id_for;
use crate::services::tui_o::writer::WriterAlarm;
use crate::services::tui_prompt_dedupe::binding_events;

const ANSWER: &str = "answered before the termination";

#[test]
#[ignore = "needs ADK_TEST_HERDR_BIN, ADK_TEST_HERDR_ROOT, ADK_TEST_HERDR_PIDS: scripts/test_p11_m2_activation.py"]
fn a_p11_m2_last_pane_close_and_exact_retire() {
    let scratch = Scratch::from_env();
    let _root = crate::config::TestRuntimeRootGuard::new();
    println!("X_TEST name=x1 pid={}", std::process::id());
    let mut s1 = Real::start(&scratch, "x1-s1");
    let mut s2 = Real::start(&scratch, "x1-s2");
    let transport = Arc::new(Witnessing::new(&s1.endpoint()));
    // O's writer hosts the channel and every thread reads one binding log.
    let events = tempfile::tempdir().unwrap();
    let events_root = events.path().to_path_buf();
    let runtime = boot(&s1, CHANNEL_ID, transport.clone(), move || {
        binding_events::set_test_root(Some(&events_root));
        let writer = [(CHANNEL_ID, RuntimeHandoffKind::ClaudeTui)];
        std::mem::forget(crate::services::tui_o::cutover::test_override::force_channels(&writer));
    });
    runtime.block_on(async {
        let (results, _logs) = capture();
        let db = TestPostgresDb::create().await;
        let pool = db.connect_and_migrate().await;
        let (shared, registry) =
            crate::services::discord::host_teardown_gate::test_support::runtime(&pool).await;
        let channel = poise::serenity_prelude::ChannelId::new(CHANNEL_ID);
        let cwd = s1.work("a");
        let logical = "AgentDesk-claude-x1";
        let target = launch(&mut s1, &pool, &shared.token_hash, CHANNEL_ID, logical, cwd).await;
        let keep = control(&mut s1, "x1-keep");
        let other = control(&mut s2, "x1-other");

        // A's transcript is its source in O's store. The writer posts one answer and is down
        // when A's sealed tail lands, so the termination meets that tail unread.
        let a_session = uuid::Uuid::new_v4().to_string();
        let a_path = root().join("a.jsonl");
        std::fs::write(&a_path, b"").unwrap();
        bind_source(logical, &a_session, &a_path);
        let a_source = source_id_for(&a_session, &a_path).unwrap();
        begin_era(&a_source, "p11-x1");
        let mut o = OHost::new();
        o.start();
        append(&a_path, &assistant("m1", ANSWER));
        o.posted(&[ANSWER]).await;
        o.stop().await;
        append(&a_path, &assistant("m2", TAIL));
        let cursor = cursor_of(&a_source);
        let written = std::fs::metadata(&a_path).unwrap().len();
        assert!(
            cursor.is_some_and(|through| through < written),
            "unread tail: {cursor:?}"
        );

        crate::services::claude::herdr_turn::hold(&target.nonce).unwrap();
        let token = test_turn::start(&shared, channel, 51, "turn-x1").await;
        let (s1_before, s2_before) = (s1.panes(), s2.panes());
        print_panes(&s1, "before", &s1_before);
        print_panes(&s2, "before", &s2_before);
        let (s1_from, s2_from) = (s1.log().len(), s2.log().len());
        let hosted: Vec<(u32, String)> = s1.recorded.iter().chain(&s2.recorded).cloned().collect();
        let o_before = o_state(events.path());
        transport.writes.lock().unwrap().clear();

        let api = serve_api(registry, pool.clone()).await;
        assert_eq!(cli_terminate(api, &target.key, &target.nonce).await, Ok(()));
        let close = the_close(&s1, &transport);
        assert_eq!(
            ROUTE_REQUESTS.load(Ordering::SeqCst),
            1,
            "one request, no retry"
        );
        let results = print_results(results.take(), &target.key, &target.nonce);
        assert_eq!(results, ["retired"]);
        // The service wrote none of O's store, cursor, ledger or sources, and posted nothing.
        let o_untouched = o_state(events.path()) == o_before && cursor_of(&a_source) == cursor;
        let service_posts = o.posts().len().saturating_sub(1);
        assert!(o_untouched, "the service wrote none of O's state");
        assert_eq!(service_posts, 0, "the service posted nothing of its own");

        // Only the target's pane and workspace are gone, with its shell and provider.
        let (s1_after, s2_after) = (s1.panes(), s2.panes());
        print_panes(&s1, "after", &s1_after);
        print_panes(&s2, "after", &s2_after);
        let kept: Vec<Value> = s1_before
            .iter()
            .filter(|pane| pane["pane_id"] != target.pane.as_str())
            .cloned()
            .collect();
        assert_eq!(s1_after, kept, "only {} left {}", target.pane, s1.name);
        assert!(s1_after.iter().any(|pane| pane["pane_id"] == keep.as_str()));
        assert!(
            s1.snapshot()["workspaces"]
                .as_array()
                .unwrap()
                .iter()
                .all(|workspace| workspace["workspace_id"] != target.workspace.as_str()),
            "the last pane took its workspace"
        );
        assert_eq!(s2_after, s2_before, "{} unchanged", s2.name);
        assert!(
            s2_after
                .iter()
                .any(|pane| pane["pane_id"] == other.as_str())
        );
        let deadline = Instant::now() + Duration::from_secs(5);
        while (alive(&target.provider) || alive(&target.shell)) && Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(!alive(&target.provider), "the recorded provider is gone");
        assert!(!alive(&target.shell));
        for process in &hosted {
            if *process != target.provider && *process != target.shell {
                assert!(alive(process), "{process:?} untouched");
            }
        }

        // One request reached S1, the close; S2 took none. Both servers still run offline.
        let requests = s1.requests_since(s1_from);
        assert_eq!(requests.len(), 1, "{requests:?}");
        assert_eq!(requests[0].0, close);
        let completed = s1.log()[s1_from..].lines().any(|line| {
            line.contains("event=\"api.request.complete\"")
                && field(line, "request_id") == close
                && field(line, "outcome") == "ok"
        });
        assert!(completed, "the close completed ok");
        let closed: Vec<String> = s1.log()[s1_from..]
            .lines()
            .filter(|line| line.contains("event=\"workspace.close\""))
            .map(|line| field(line, "workspace_id"))
            .collect();
        assert_eq!(closed, std::slice::from_ref(&target.workspace));
        assert_eq!(s2.requests_since(s2_from), Vec::new());
        assert!(alive(&(s1.pid, s1.start.clone())) && alive(&(s2.pid, s2.start.clone())));
        s1.assert_offline();
        s2.assert_offline();

        // The row retired exactly this nonce, the hold is gone and the captured turn settled.
        let record = stored(&pool, &target.key).await;
        assert_eq!(
            (record.state, record.execution_nonce.as_str()),
            (HostedState::Retired, target.nonce.as_str())
        );
        let holds = crate::services::claude::herdr_turn::input_holds().unwrap();
        assert!(holds.iter().all(|(nonce, _)| *nonce != target.nonce));
        let active = test_turn::active(&shared, channel).await;
        assert!(active.is_none_or(|now| !Arc::ptr_eq(&now, &token)));
        assert!(!token.cancelled.load(Ordering::Relaxed));
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);

        // O, back, delivers the sealed tail once. A restart posts its own new output once and
        // nothing again.
        o.start();
        let delivered = o.until(|posts| posts.len() >= 2).await;
        o.stop().await;
        let (posts, ledger) = (o.posts(), posted_in_ledger(TAIL));
        o.start();
        append(&a_path, &assistant("c1", RESTART_OUTPUT));
        let restarted = o
            .until(|posts| posts.iter().any(|p| p == RESTART_OUTPUT))
            .await;
        o.stop().await;
        let (after, halts) = (o.posts(), o.halts());
        let count =
            |posts: &[String], payload: &str| posts.iter().filter(|p| *p == payload).count();
        let restart_output = count(&after, RESTART_OUTPUT);
        let reposts = after.len() - posts.len() - restart_output;
        println!(
            "X_O name=x1 tail=unread service_o_writes={} service_posts={service_posts} \
             tail_posts={} tail_ledger_posted={ledger} restart_output={restart_output} \
             restart_reposts={reposts} halts={}",
            usize::from(!o_untouched),
            count(&posts, TAIL),
            halts.len()
        );
        assert!(delivered, "{posts:?} {halts:?}");
        assert_eq!(posts, [ANSWER, TAIL], "{halts:?}");
        assert_eq!(ledger, 1, "the tail is delivered once");
        assert!(restarted, "the restart posts its new output: {halts:?}");
        assert_eq!(
            after,
            [ANSWER, TAIL, RESTART_OUTPUT],
            "a restart reposts nothing"
        );
        assert_eq!(posted_in_ledger(TAIL), 1);
        assert_eq!(posted_in_ledger(RESTART_OUTPUT), 1);
        assert_eq!(halts, Vec::<WriterAlarm>::new());
        println!("X_EFFECT name=x1 row=retired hold=released turn=settled");
        pool.close().await;
        db.drop().await;
    });
}
