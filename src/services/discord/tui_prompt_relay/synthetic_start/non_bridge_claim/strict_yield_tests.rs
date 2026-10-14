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
        // A bridge-owned copy reaches the exact-lease compare whatever the session-bound flag is.
        for older in [&turn.lease, &hook_lease] {
            let bridge_owned = ExternalInputRelayLease {
                relay_owner: ExternalInputRelayOwner::BridgeAdapter,
                ..older.clone()
            };
            for held in [older, &bridge_owned] {
                drop(
                    super::super::super::relay_ownership::TuiDirectExternalInputLeaseGuard::new(
                        ProviderKind::Claude,
                        &turn.tmux,
                        turn.channel,
                        held,
                    ),
                );
            }
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

/// The wiring holds the channel lock from its claim through its owner adopt. A hand-off that
/// starts in between cannot commit first, so the late adopt never lands on a re-owned turn.
#[test]
fn reown_cannot_commit_between_a_claim_and_its_owner_adopt() {
    isolated(|dir| async move {
        let (b, end) = transcript(&dir, "adopt-race.jsonl", "");
        // Observed with no watcher, the hook lease is the bridge's; the claim then delegates to
        // the watcher that appeared, which the wiring has to adopt into the lease.
        let turn = Turn::observe(170, &b, end, Some(&b), None);
        assert_eq!(
            turn.lease.relay_owner,
            ExternalInputRelayOwner::BridgeAdapter
        );
        turn.watch(b.to_str().expect("utf8 path"));
        let adopt_pause = &super::super::super::synthetic_start_wiring::ADOPT_PAUSE;
        let (claimed, adopt) = arm(adopt_pause, turn.channel);
        let wiring = turn.wire();
        tokio::pin!(wiring);
        tokio::select! {
            _ = claimed.notified() => {}
            _ = &mut wiring => panic!("the wiring adopts the claim's watcher owner"),
        }
        assert_eq!(
            turn.row().effective_relay_owner_kind(),
            RelayOwnerKind::Watcher
        );
        turn.stop_watcher();

        // Virtual time runs the hand-off through its refusal until it parks with no timer left.
        tokio::time::pause();
        let handoff = turn.hand_off(&b);
        tokio::pin!(handoff);
        let overtook = tokio::select! {
            biased;
            handoff = &mut handoff => Some(handoff),
            _ = tokio::time::sleep(Duration::from_secs(60)) => None,
        };
        tokio::time::resume();
        adopt.notify_one();
        let adopted = wiring.await;
        assert_eq!(adopted.relay_owner, ExternalInputRelayOwner::TmuxWatcher);
        let overtaken = overtook.is_some();
        let handoff = match overtook {
            Some(handoff) => handoff,
            None => handoff.await,
        };
        let NonBridgeHandoff::Reowned(current) = handoff else {
            panic!("the refused turn is re-owned: {handoff:?}");
        };
        assert_eq!(
            turn.live_lease(),
            Some(current.clone()),
            "the re-own's bridge lease is the last one stored"
        );
        assert!(turn.capture(&b, &current).await.is_ok(), "capture");
        assert!(!overtaken, "the re-own finished inside the claim's lock");
    });
}

/// The real Claude idle caller observes the prompt with no row yet, the hook wiring then
/// delegates the turn to a watcher that dies, and the caller tails the re-owned turn under the
/// lease the re-own stored: the tail's guard releases that lease when the tail ends.
#[test]
fn claude_idle_caller_tails_a_reowned_turn_under_the_stored_lease() {
    isolated(|dir| async move {
        // Session-bound delivery lets the caller scan beside a live watcher.
        enable_session_bound_delivery();
        let (b, end) = transcript(&dir, "caller.jsonl", "");
        let turn = Turn::observe(180, &b, end, Some(&b), Some(&b));
        // The tmux session is gone, so the tail the caller starts ends on its own.
        let tmux =
            crate::services::provider_teardown::tests::test_support::FakeTmux::install(&turn.tmux);
        tmux.mark_missing();
        crate::services::tui_prompt_dedupe::clear_external_input_relay_lease(
            "claude",
            &turn.tmux,
            turn.channel.get(),
        );
        let mut prompts = crate::services::tui_prompt_dedupe::subscribe_observed_prompts();
        let (entered, resume) = arm(&HANDOFF_PAUSE, turn.channel);
        let caller =
            super::super::super::claude_idle_runtime::relay_idle_claude_bindings(&turn.shared);
        tokio::pin!(caller);
        tokio::select! {
            _ = entered.notified() => {}
            _ = &mut caller => panic!("the caller reaches the hand-off"),
        }
        let no_row =
            inflight::load_inflight_state_read_only(&ProviderKind::Claude, turn.channel.get());
        assert!(no_row.is_none(), "the caller came through with no row");
        let observed = loop {
            let prompt = prompts.try_recv().expect("the caller published its prompt");
            if prompt.tmux_session_name == turn.tmux {
                break prompt;
            }
        };
        let observer = turn.live_lease().expect("the caller's lease");

        // The hook relay's half, on the event the caller published.
        let mut hook_lease =
            super::super::super::relay_ownership::record_observed_external_turn_lease(
                &turn.shared,
                &observed,
                turn.channel,
            );
        assert_eq!(
            hook_lease.turn_id, observer.turn_id,
            "the hook and the caller name one turn"
        );
        let decision = super::super::super::observed_prompt_decision::relay_observed_prompt_injected_prompt_decision(&observed.prompt);
        let deferred =
            super::super::super::synthetic_start_wiring::wire_tui_direct_synthetic_turn_start(
                &turn.shared,
                "claude",
                turn.channel,
                &observed,
                turn.anchor(),
                false,
                &decision,
                &mut hook_lease,
            )
            .await;
        assert!(!deferred, "no prior turn, so the claim runs inline");
        assert_ne!(
            turn.row().effective_relay_owner_kind(),
            RelayOwnerKind::None,
            "the claim delegated the turn"
        );
        turn.stop_watcher();
        resume.notify_one();
        caller.await;

        let current = turn.live_lease().expect("stored lease");
        assert_eq!(current.relay_owner, ExternalInputRelayOwner::BridgeAdapter);
        assert_eq!(current.turn_id, observer.turn_id);
        assert_eq!(
            turn.row().effective_relay_owner_kind(),
            RelayOwnerKind::None
        );
        let tailing = || {
            let tails = super::super::super::CLAUDE_IDLE_RESPONSE_TAILS.lock();
            (tails.unwrap_or_else(|poison| poison.into_inner())).contains(&turn.tmux)
        };
        assert!(tailing(), "the caller tails the turn");
        let ended = tokio::time::timeout(Duration::from_secs(30), async {
            while tailing() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });
        assert!(
            ended.await.is_ok(),
            "the tail ends once the session is gone"
        );
        assert_eq!(
            turn.live_lease(),
            None,
            "the tail released the lease it was started under"
        );
    });
}

/// The bridge tail of a re-owned turn admits its native terminal, which stamps the native
/// session on the row. The witness the re-own recorded follows that write, so the turn is
/// still resumable by its own actor afterwards.
#[test]
fn reowned_turn_keeps_its_bridge_witness_through_a_native_terminal_admission() {
    use std::os::unix::fs::MetadataExt;
    isolated(|dir| async move {
        let dir = std::fs::canonicalize(dir).expect("canonical dir");
        let body = "{\"type\":\"assistant\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"B\"}]}}\n";
        let (b, end) = transcript(&dir, "admitted.jsonl", body);
        let turn = Turn::observe(190, &b, end, Some(&b), Some(&b));
        turn.wire().await;
        turn.stop_watcher();
        let NonBridgeHandoff::Reowned(current) = turn.hand_off(&b).await else {
            panic!("re-owned");
        };
        let claim = turn.capture(&b, &current).await.expect("capture");
        let generation = crate::services::tmux_common::session_temp_path(&turn.tmux, "generation");
        std::fs::write(generation, b"1").expect("tmux generation");
        let file = std::fs::metadata(&b).expect("transcript");
        let frame = StreamMessage::ClaudeTuiTerminalDone {
            result: "B".to_string(),
            session_id: Some("native-session-5808".to_string()),
            transcript_path: b.display().to_string(),
            tmux_session_name: turn.tmux.clone(),
            turn_nonce: claim.row.turn_nonce.clone().expect("turn nonce"),
            source_start: claim.row.turn_start_offset.expect("turn start"),
            complete_record_end: file.len(),
            generation_mtime_ns:
                crate::services::discord::turn_bridge::tmux_generation_file_mtime_ns(&turn.tmux),
            source_file_dev: file.dev(),
            source_file_ino: file.ino(),
            actor: Arc::downgrade(&claim.actor),
            kind: crate::services::agent_protocol::NativeTerminalKind::Completed,
        };
        let (mut local, mut baseline) = (claim.row.clone(), claim.row.clone());
        let admitted = local
            .admit_tui_terminal_frame(
                &mut baseline,
                &inflight::InflightTurnIdentity::from_state(&claim.row),
                true,
                (&turn.shared, &claim.actor),
                "B",
                frame,
            )
            .await;
        assert!(admitted.is_ok(), "the terminal is admitted: {admitted:?}");
        let row = turn.row();
        assert_eq!(row.session_id.as_deref(), Some("native-session-5808"));
        drop(claim);
        let resumed = super::super::bridge_handoff::capture_dormant_partial(&turn.shared, &row, &b);
        assert!(
            resumed.await.is_some(),
            "the admitted row resumes on its witness"
        );
    });
}

#[derive(Clone, Default)]
struct Logs(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        (self.0.lock().unwrap_or_else(|poison| poison.into_inner())).extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Logs {
    type Writer = Self;

    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// The settle and hand-off logs keep the source match, the recheck and the outcome apart, so
/// a yield, a re-own and a refusal read differently.
#[test]
fn the_hand_off_logs_tell_source_match_recheck_and_outcome_apart() {
    let logs = Logs::default();
    crate::logging::test_capture::pin_callsite_interest();
    let _capture = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_ansi(false)
            .without_time()
            .with_writer(logs.clone())
            .finish(),
    );
    isolated(|dir| async move {
        let (b, end) = transcript(&dir, "logged-yield.jsonl", "");
        let turn = Turn::observe(130, &b, end, Some(&b), Some(&b));
        turn.claim().await;
        assert!(matches!(turn.hand_off(&b).await, NonBridgeHandoff::Yielded));
        let (c, end) = transcript(&dir, "logged-reown.jsonl", "");
        let turn = Turn::observe(131, &c, end, Some(&c), Some(&c));
        turn.wire().await;
        turn.stop_watcher();
        assert!(matches!(
            turn.hand_off(&c).await,
            NonBridgeHandoff::Reowned(_)
        ));
    });
    let text = String::from_utf8(logs.0.lock().expect("logs").clone()).expect("utf8 logs");
    let line = |needle: &str| {
        (text.lines().filter(|line| line.contains(needle)))
            .collect::<Vec<_>>()
            .join("\n")
    };
    let settled = line("settled whether a non-bridge owner reads the expected source");
    for field in [
        "expected_row_match=Match",
        "expected_watcher_match=Match",
        "row_revalidated=true",
        "watcher_revalidated=true",
        "yield_allowed=true",
        "row_revalidated=false",
        "watcher_live=false",
        "yield_allowed=false",
        "reason=\"watcher is not relay-live\"",
    ] {
        assert!(settled.contains(field), "{field}: {settled}");
    }
    let handed = line("handed off its observed turn");
    for field in [
        "handoff=\"yielded\"",
        "handoff=\"reowned\"",
        "reason=\"re-owned\"",
    ] {
        assert!(handed.contains(field), "{field}: {handed}");
    }
    assert!(
        line("re-owned a refused TUI-direct delegation").contains("previous_output="),
        "{text}"
    );
}
