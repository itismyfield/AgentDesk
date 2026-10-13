//! The strict non-bridge yield and its hand-off, on rows the real claim and wiring made.
use super::super::bridge_handoff::reown::delegated_proof_held;
use super::super::bridge_handoff::reown::reown_tests::{
    Turn, alias, arm, enable_session_bound_delivery, isolated, transcript,
};
use super::*;
use crate::services::discord::inflight;
use crate::services::discord::session_relay_sink::tests::idle_relay_harness::live_watcher;

const OWNER_ELSEWHERE: &str = "owner does not read the expected source";

fn refused(wait: NonBridgeWait) -> (Option<InflightEpisodePin>, &'static str) {
    match wait {
        NonBridgeWait::Refused { own_row, reason } => (own_row.map(|pin| *pin), reason),
        NonBridgeWait::Yielded => panic!("yielded"),
    }
}

fn deregister_producer(turn: &Turn) {
    crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
        .deregister(&turn.tmux);
}

/// The wait yields only when this turn's owner and its live watcher both resolve to the
/// observed source, aliases included; a yielding hand-off leaves the delegated row alone.
#[test]
fn strict_wait_yields_to_an_owner_reading_the_observed_source() {
    isolated(|dir| async move {
        // Watcher row and watcher on B, observed on B or through an alias of B.
        let (b, end) = transcript(&dir, "b.jsonl", "");
        let b_alias = alias(&dir, "b-alias.jsonl", &b);
        let turn = Turn::observe(1, &b, end, Some(&b), Some(&b));
        assert_eq!(turn.claim().await, ExternalInputRelayOwner::TmuxWatcher);
        for expected in [&b, &b_alias] {
            let wait = turn.wait(expected, turn.turn_id()).await;
            assert!(matches!(wait, NonBridgeWait::Yielded));
        }
        turn.watch(b_alias.to_str().unwrap());
        let wait = turn.wait(&b, turn.turn_id()).await;
        assert!(matches!(wait, NonBridgeWait::Yielded));
        let before = turn.row_json();
        assert!(matches!(turn.hand_off(&b).await, NonBridgeHandoff::Yielded));
        assert_eq!(turn.row_json(), before, "a yield writes nothing");
        assert!(delegated_proof_held(&ProviderKind::Claude, turn.channel));

        // Session-bound row on C read by a live watcher on an alias of C, producer live.
        enable_session_bound_delivery();
        let (c, end) = transcript(&dir, "c.jsonl", "");
        let c_alias = alias(&dir, "c-alias.jsonl", &c);
        let turn = Turn::observe(2, &c, end, Some(&c), Some(&c_alias));
        let _producer = turn.producer();
        let owner = turn.claim().await;
        assert_eq!(owner, ExternalInputRelayOwner::SessionBoundRelay);
        let wait = turn.wait(&c, turn.turn_id()).await;
        assert!(matches!(wait, NonBridgeWait::Yielded));
        deregister_producer(&turn);
        let (own_row, reason) = refused(turn.wait(&c, turn.turn_id()).await);
        assert_eq!(reason, "session-bound owner has no live producer");
        assert!(own_row.is_some());
    });
}

/// A delegated row this turn cannot prove is read from `E` is refused, and `own_row` is given
/// only for this turn's own row: another source, an unknown or dead watcher, another turn.
#[test]
fn strict_wait_refuses_an_owner_not_proven_to_read_the_observed_source() {
    isolated(|dir| async move {
        let (a, _) = transcript(&dir, "a.jsonl", "");
        let mut n = 10;
        let mut case = |bound: bool| {
            n += 1;
            let (path, end) = transcript(&dir, &format!("case-{n}.jsonl"), "");
            Turn::observe(n, &path, end, bound.then_some(path.as_path()), Some(&path))
        };

        // Watcher row on B whose live watcher now reads A.
        let turn = case(true);
        turn.claim().await;
        turn.watch(a.to_str().unwrap());
        let (own_row, reason) = refused(turn.wait(&turn.source, turn.turn_id()).await);
        assert_eq!((own_row.is_some(), reason), (true, OWNER_ELSEWHERE));

        // No output on the row (no binding), an empty watcher path, an unresolvable E.
        let turn = case(false);
        turn.claim().await;
        assert_eq!(turn.row().output_path, None);
        let (_, reason) = refused(turn.wait(&turn.source, turn.turn_id()).await);
        assert_eq!(reason, OWNER_ELSEWHERE);
        let turn = case(true);
        turn.claim().await;
        turn.watch("");
        let (_, reason) = refused(turn.wait(&turn.source, turn.turn_id()).await);
        assert_eq!(reason, OWNER_ELSEWHERE);
        let missing = dir.join("missing.jsonl");
        let turn = Turn::observe(30, &missing, 0, Some(&missing), Some(&missing));
        turn.claim().await;
        assert_eq!(turn.row().output_path.as_deref(), missing.to_str());
        let (_, reason) = refused(turn.wait(&missing, turn.turn_id()).await);
        assert_eq!(reason, OWNER_ELSEWHERE);

        // A cancelled and a heartbeat-stale watcher on the very same file.
        for stale in [false, true] {
            let turn = case(true);
            turn.claim().await;
            if stale {
                turn.stop_watcher();
            } else {
                let mut cancelled = live_watcher(&turn.tmux, turn.source.to_str().unwrap());
                cancelled.cancel = Arc::new(AtomicBool::new(true));
                turn.shared.tmux_watchers.insert(turn.channel, cancelled);
            }
            let (own_row, reason) = refused(turn.wait(&turn.source, turn.turn_id()).await);
            assert_eq!(
                (own_row.is_some(), reason),
                (true, "watcher is not relay-live")
            );
        }

        // Another turn's row on the same source with a live watcher, and a bridge-owned row.
        let turn = case(true);
        turn.claim().await;
        let (own_row, reason) = refused(turn.wait(&turn.source, "external:claude:other").await);
        assert_eq!(
            (own_row.is_none(), reason),
            (true, "row belongs to another turn")
        );
        let (path, end) = transcript(&dir, "bridge-owned.jsonl", "");
        let turn = Turn::observe(31, &path, end, Some(&path), None);
        assert_eq!(turn.claim().await, ExternalInputRelayOwner::BridgeAdapter);
        let (own_row, reason) = refused(turn.wait(&turn.source, turn.turn_id()).await);
        assert_eq!((own_row.is_none(), reason), (true, "row is not delegated"));
    });
}

/// The yield is decided on the row and watcher handle it read: a row replaced or moved to
/// another source, or a watcher handle replaced, before the recheck cannot yield on that read.
#[test]
fn strict_wait_does_not_yield_on_a_row_or_watcher_that_changed_before_the_recheck() {
    isolated(|dir| async move {
        let (other, _) = transcript(&dir, "other.jsonl", "");
        for (n, change) in [(41, "source moved"), (42, "row replaced")] {
            let (b, end) = transcript(&dir, &format!("recheck-{n}.jsonl"), "");
            let turn = Turn::observe(n, &b, end, Some(&b), Some(&b));
            turn.claim().await;
            let (entered, resume) = arm(&REVALIDATE_PAUSE, turn.channel);
            let wait = turn.wait(&b, turn.turn_id());
            tokio::pin!(wait);
            tokio::select! {
                _ = entered.notified() => {}
                _ = &mut wait => panic!("{change}: the first poll reaches the recheck"),
            }
            let mut successor = turn.row();
            if change == "source moved" {
                successor.output_path = other.to_str().map(str::to_string);
            } else {
                successor.external_turn_id = Some("external:claude:successor".to_string());
            }
            inflight::save_inflight_state(&successor).expect("successor");
            resume.notify_one();
            let wait = wait.await;
            let expected = if change == "source moved" {
                OWNER_ELSEWHERE
            } else {
                "row belongs to another turn"
            };
            assert_eq!(refused(wait).1, expected, "{change}");
            assert_eq!(
                (turn.row().output_path, turn.row().external_turn_id),
                (successor.output_path, successor.external_turn_id),
                "{change}: the successor is kept"
            );
        }

        // The watcher handle is replaced by one on the same file: the read it rechecks is gone.
        let (b, end) = transcript(&dir, "recheck-43.jsonl", "");
        let turn = Turn::observe(43, &b, end, Some(&b), Some(&b));
        turn.claim().await;
        let (entered, resume) = arm(&REVALIDATE_PAUSE, turn.channel);
        let wait = turn.wait(&b, turn.turn_id());
        tokio::pin!(wait);
        tokio::select! {
            _ = entered.notified() => {}
            _ = &mut wait => panic!("the first poll reaches the recheck"),
        }
        turn.watch(b.to_str().unwrap());
        let (again, resume_again) = arm(&REVALIDATE_PAUSE, turn.channel);
        resume.notify_one();
        tokio::select! {
            _ = again.notified() => {}
            _ = &mut wait => panic!("the replaced handle must not pass the first recheck"),
        }
        turn.stop_watcher();
        resume_again.notify_one();
        let (own_row, reason) = refused(wait.await);
        assert_eq!(
            (own_row.is_some(), reason),
            (true, "watcher is not relay-live")
        );
    });
}

/// Progress the live turn persists between the read and the recheck does not cost the yield.
#[test]
fn strict_wait_yields_through_progress_on_the_same_episode() {
    isolated(|dir| async move {
        let (b, end) = transcript(&dir, "progress.jsonl", "");
        let turn = Turn::observe(50, &b, end, Some(&b), Some(&b));
        turn.claim().await;
        let (entered, resume) = arm(&REVALIDATE_PAUSE, turn.channel);
        let wait = turn.wait(&b, turn.turn_id());
        tokio::pin!(wait);
        tokio::select! {
            _ = entered.notified() => {}
            _ = &mut wait => panic!("the first poll reaches the recheck"),
        }
        let mut progressed = turn.row();
        progressed.current_msg_id = turn.anchor().get() + 7;
        progressed.last_offset += 64;
        progressed.full_response = "partial answer".to_string();
        progressed.session_id = Some("native-session-5808".to_string());
        inflight::save_inflight_state(&progressed).expect("progress");
        let (again, resume_again) = arm(&REVALIDATE_PAUSE, turn.channel);
        resume.notify_one();
        let yielded = tokio::select! {
            wait = &mut wait => matches!(wait, NonBridgeWait::Yielded),
            _ = again.notified() => false,
        };
        resume_again.notify_one();
        assert!(yielded, "the first recheck yields despite progress");
    });
}

/// The wiring adopts a watcher owner and re-records the lease, the watcher dies, and the
/// refused turn is re-owned with a fresh BridgeAdapter lease that the older guards keep.
#[test]
fn refused_watcher_turn_is_reowned_for_the_bridge_after_its_watcher_dies() {
    isolated(|dir| async move {
        let (b, end) = transcript(&dir, "wired.jsonl", "");
        // The observer's own lease owner follows the process-wide session-bound flag; the claim's does not.
        let turn = Turn::observe(60, &b, end, Some(&b), Some(&b));
        let hook_lease = turn.wire().await;
        assert_eq!(hook_lease.relay_owner, ExternalInputRelayOwner::TmuxWatcher);
        assert_eq!(turn.live_lease(), Some(hook_lease.clone()));
        turn.stop_watcher();

        let NonBridgeHandoff::Reowned(current) = turn.hand_off(&b).await else {
            panic!("the refused turn is re-owned");
        };
        assert_eq!(current.relay_owner, ExternalInputRelayOwner::BridgeAdapter);
        assert_eq!(current.turn_id, turn.lease.turn_id);
        assert_eq!(turn.live_lease(), Some(current.clone()), "the stored lease");
        let row = turn.row();
        assert_eq!(row.effective_relay_owner_kind(), RelayOwnerKind::None);
        assert_eq!(row.output_path.as_deref(), b.to_str());
        assert_eq!(row.turn_start_offset, Some(0), "same file keeps its start");
        assert!(!delegated_proof_held(&ProviderKind::Claude, turn.channel));

        // Guards still holding the observer's and the wiring's older leases clear nothing now.
        drop(
            super::super::super::relay_ownership::TuiDirectObservedLeaseEarlyReturnGuard::arm(
                "claude",
                &turn.tmux,
                turn.channel,
                hook_lease.generation,
            ),
        );
        for older in [&turn.lease, &hook_lease] {
            drop(
                super::super::super::relay_ownership::TuiDirectExternalInputLeaseGuard::new(
                    ProviderKind::Claude,
                    &turn.tmux,
                    turn.channel,
                    older,
                ),
            );
        }
        assert_eq!(turn.live_lease(), Some(current.clone()));
        let claim = turn.capture(&b, &current).await;
        assert!(claim.is_ok(), "capture takes the re-owned turn");
    });
}

/// A session-bound row on another file A is re-owned onto the observed B from the prompt end;
/// an alias of the row's own file keeps the start and only takes the observed spelling.
#[test]
fn reown_restamps_another_source_from_the_prompt_end_and_keeps_an_alias_start() {
    isolated(|dir| async move {
        enable_session_bound_delivery();
        let (a, _) = transcript(&dir, "sbr-a.jsonl", "");
        let a_alias = alias(&dir, "sbr-a-alias.jsonl", &a);
        let body = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"B\"}]}}\n";
        let (b, end) = transcript(&dir, "sbr-b.jsonl", body);
        let turn = Turn::observe(70, &b, end, Some(&a), Some(&a_alias));
        let _producer = turn.producer();
        let owner = turn.claim().await;
        assert_eq!(owner, ExternalInputRelayOwner::SessionBoundRelay);
        assert_eq!(turn.row().output_path.as_deref(), a.to_str());

        let NonBridgeHandoff::Reowned(current) = turn.hand_off(&b).await else {
            panic!("a session-bound row on another file is re-owned");
        };
        let row = turn.row();
        assert_eq!(row.output_path.as_deref(), b.to_str());
        assert_eq!((row.turn_start_offset, row.last_offset), (Some(end), end));
        assert!(
            std::fs::metadata(&b).unwrap().len() > end,
            "B has a body past the prompt"
        );
        assert!(turn.capture(&b, &current).await.is_ok());
        deregister_producer(&turn);

        // Watcher row on C observed through an alias of C, its watcher dead.
        let (c, end) = transcript(&dir, "alias-c.jsonl", body);
        let c_alias = alias(&dir, "alias-c-link.jsonl", &c);
        let turn = Turn::observe(71, &c_alias, end, Some(&c), Some(&c));
        turn.claim().await;
        turn.stop_watcher();
        let NonBridgeHandoff::Reowned(current) = turn.hand_off(&c_alias).await else {
            panic!("an aliased refused turn is re-owned");
        };
        let row = turn.row();
        assert_eq!(row.output_path.as_deref(), c_alias.to_str());
        assert_eq!(
            row.turn_start_offset,
            Some(0),
            "the same file keeps its start"
        );
        assert!(turn.capture(&c_alias, &current).await.is_ok());
    });
}

/// A re-owned turn whose tail fails, before or after capture stamped a native session on the
/// row, resumes through the dormant consumer on the witness the re-own left behind.
#[test]
fn reowned_turn_resumes_through_the_dormant_consumer_after_a_failed_tail() {
    isolated(|dir| async move {
        let (b, end) = transcript(&dir, "dormant.jsonl", "");
        let turn = Turn::observe(90, &b, end, Some(&b), Some(&b));
        turn.wire().await;
        turn.stop_watcher();
        let NonBridgeHandoff::Reowned(current) = turn.hand_off(&b).await else {
            panic!("re-owned");
        };
        // The tail dies before capture.
        let resumed = turn.resume_unpublished(&turn.shared).await;
        assert_eq!(
            resumed.map(|lease| lease.relay_owner),
            Some(ExternalInputRelayOwner::BridgeAdapter)
        );

        // The tail captures, stamps the native session on a stream tick, then dies.
        let claim = turn.capture(&b, &current).await.expect("capture");
        let mut baseline = claim.row.clone();
        let mut stamped = baseline.clone();
        stamped.session_id = Some("native-session-5808".to_string());
        assert_eq!(
            inflight::save_stream_tick_state_preserving_current_message_races(
                &mut baseline,
                &mut stamped,
                &inflight::InflightTurnIdentity::from_state(&claim.row),
                claim.row.current_msg_id,
                claim.row.current_msg_len,
                "test::reowned_stream_tick",
            ),
            inflight::GuardedSaveOutcome::Saved
        );
        drop(claim);
        assert!(turn.resume_unpublished(&turn.shared).await.is_some());
    });
}
