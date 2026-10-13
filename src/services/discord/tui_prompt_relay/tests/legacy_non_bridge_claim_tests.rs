//! The strict non-bridge yield and the refused-delegation re-own, on rows the real claim made.
use super::synthetic_start::bridge_handoff::{
    self, BridgeClaim,
    reown::{ObservedTurn, REOWN_PAUSE, delegated_proof_held, reown_refused_delegation},
};
use super::synthetic_start::non_bridge_claim::{
    NonBridgeHandoff, NonBridgeWait, REVALIDATE_PAUSE, hand_off_observed_turn,
    wait_for_source_bound_non_bridge_claim,
};
use super::*;
use crate::services::cluster::stream_relay::{
    RelaySink, RelaySinkError, RelaySinkOutcome, StreamFrame, StreamRelayHandle,
};
use crate::services::discord::inflight::{self, InflightEpisodePin};
use crate::services::discord::session_relay_sink::tests::idle_relay_harness::live_watcher;
use std::future::Future;
use tokio::sync::Notify;

const PROMPT: &str = "please review the 5808 handoff";
const CLAUDE: &str = "claude";

type PauseSlot = Mutex<Option<(u64, Arc<Notify>, Arc<Notify>)>>;

/// Runs `body` with the dedupe maps to itself, under its own runtime root.
fn isolated<F: Future<Output = ()>>(body: impl FnOnce(PathBuf) -> F) {
    let root = tempfile::tempdir().expect("runtime root");
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _dedupe = crate::services::tui_prompt_dedupe::TEST_LOCK
        .lock()
        .unwrap_or_else(|poison| poison.into_inner());
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(body(root.path().to_path_buf()));
    for slot in [&REVALIDATE_PAUSE, &REOWN_PAUSE] {
        *slot.lock().unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}

/// A transcript holding the observed prompt, then `body`; returns it and the prompt end.
fn transcript(dir: &Path, name: &str, body: &str) -> (PathBuf, u64) {
    let path = dir.join(name);
    let prompt = format!(
        "{}\n",
        serde_json::json!({"type": "user", "message": {"content": PROMPT}})
    );
    std::fs::write(&path, format!("{prompt}{body}")).expect("transcript");
    (path, prompt.len() as u64)
}

fn alias(dir: &Path, name: &str, target: &Path) -> PathBuf {
    let path = dir.join(name);
    std::os::unix::fs::symlink(target, &path).expect("alias");
    path
}

fn arm(slot: &'static PauseSlot, channel: ChannelId) -> (Arc<Notify>, Arc<Notify>) {
    let (entered, resume) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    *slot.lock().unwrap_or_else(|poison| poison.into_inner()) =
        Some((channel.get(), entered.clone(), resume.clone()));
    (entered, resume)
}

fn enable_session_bound_delivery() {
    let health = Arc::new(crate::services::discord::health::HealthRegistry::new());
    crate::services::discord::session_relay_sink::SessionBoundDiscordRelaySink::new(health)
        .enable_delivery_for_test();
}

struct NullSink;

#[async_trait::async_trait]
impl RelaySink for NullSink {
    async fn deliver(&self, _frame: &StreamFrame) -> Result<RelaySinkOutcome, RelaySinkError> {
        Ok(RelaySinkOutcome::FrameAccepted)
    }
}

/// One Claude prompt the idle observer saw on `source` in its own session and channel.
struct Turn {
    shared: Arc<SharedData>,
    channel: ChannelId,
    tmux: String,
    source: PathBuf,
    prompt_end: u64,
    lease: ExternalInputRelayLease,
}

impl Turn {
    /// Binds `bound` as the session transcript and records the observer's lease for `source`.
    fn observe(n: u64, source: &Path, prompt_end: u64, bound: Option<&Path>) -> Self {
        let shared = crate::services::discord::make_shared_data_for_tests();
        let channel = ChannelId::new(5_808_000_000 + n * 10);
        let tmux = format!("AgentDesk-claude-5808-s0-{n}");
        if let Some(bound) = bound {
            crate::services::tui_prompt_dedupe::register_tmux_runtime_binding(
                &tmux,
                crate::services::tui_prompt_dedupe::TuiRuntimeBinding {
                    runtime_kind: RuntimeHandoffKind::ClaudeTui,
                    output_path: bound.to_str().expect("utf8 path").to_string(),
                    relay_output_path: None,
                    input_fifo_path: None,
                    session_id: None,
                    last_offset: 0,
                    relay_last_offset: None,
                },
            );
        }
        let lease = super::super::relay_ownership::record_external_turn_lease_for_output(
            &shared,
            &ProviderKind::Claude,
            channel,
            &tmux,
            RuntimeHandoffKind::ClaudeTui,
            source,
            chrono::Utc::now(),
        );
        Self {
            shared,
            channel,
            tmux,
            source: source.to_path_buf(),
            prompt_end,
            lease,
        }
    }

    fn anchor(&self) -> MessageId {
        MessageId::new(self.channel.get() + 1)
    }

    fn turn_id(&self) -> &str {
        self.lease.turn_id.as_deref().expect("observed turn id")
    }

    fn watch(&self, path: &str) -> Arc<AtomicBool> {
        let watcher = live_watcher(&self.tmux, path);
        let cancel = watcher.cancel.clone();
        self.shared.tmux_watchers.insert(self.channel, watcher);
        cancel
    }

    async fn claim(&self) -> ExternalInputRelayOwner {
        let claim = synthetic_start::claim_tui_direct_synthetic_turn(
            &self.shared,
            &ProviderKind::Claude,
            self.channel,
            &self.tmux,
            PROMPT,
            self.anchor(),
            &self.lease,
        )
        .await;
        assert!(claim.claimed, "fixture claim");
        claim.relay_owner
    }

    /// Claims through the real hook wiring, which re-records the hook lease to the claim's owner.
    async fn wire(&self) -> ExternalInputRelayLease {
        let prompt = ObservedTuiPrompt {
            provider: CLAUDE.to_string(),
            tmux_session_name: self.tmux.clone(),
            prompt: PROMPT.to_string(),
            source_event_id: None,
            observed_at: chrono::Utc::now(),
            external_input_lease_generation: self.lease.generation,
            ssh_direct_observation_generation:
                crate::services::tui_prompt_dedupe::SSH_DIRECT_OBSERVATION_GENERATION_UNRECORDED,
            hook_prompt_id: None,
            row_prompt_id: None,
            native_turn_id: None,
            steer_echo: false,
        };
        let mut hook_lease = self.lease.clone();
        let deferred = synthetic_start_wiring::wire_tui_direct_synthetic_turn_start(
            &self.shared,
            CLAUDE,
            self.channel,
            &prompt,
            self.anchor(),
            false,
            &relay_observed_prompt_injected_prompt_decision(PROMPT),
            &mut hook_lease,
        )
        .await;
        assert!(!deferred, "no prior turn, so the claim runs inline");
        hook_lease
    }

    fn row(&self) -> InflightTurnState {
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, self.channel.get())
            .expect("claimed row")
    }

    fn row_json(&self) -> serde_json::Value {
        serde_json::to_value(self.row()).expect("row json")
    }

    fn live_lease(&self) -> Option<ExternalInputRelayLease> {
        crate::services::tui_prompt_dedupe::external_input_relay_lease(
            CLAUDE,
            &self.tmux,
            self.channel.get(),
        )
    }

    async fn wait(&self, expected: &Path, turn_id: &str) -> NonBridgeWait {
        wait_for_source_bound_non_bridge_claim(
            &self.shared.tmux_watchers,
            &ProviderKind::Claude,
            self.channel,
            &self.tmux,
            expected,
            turn_id,
        )
        .await
    }

    async fn hand_off(&self, expected: &Path) -> NonBridgeHandoff {
        hand_off_observed_turn(
            &self.shared,
            &ProviderKind::Claude,
            self.channel,
            &self.tmux,
            expected,
            self.prompt_end,
            &self.lease,
        )
        .await
    }

    async fn reown(&self) -> Result<ExternalInputRelayLease, &'static str> {
        let provider = ProviderKind::Claude;
        let turn = ObservedTurn {
            provider: &provider,
            channel: self.channel,
            tmux: &self.tmux,
            expected_output: &self.source,
            prompt_end: self.prompt_end,
            lease: &self.lease,
        };
        let own_row = InflightEpisodePin::from_state(&self.row());
        reown_refused_delegation(&self.shared, &turn, &own_row).await
    }

    async fn capture(
        &self,
        output: &Path,
        lease: &ExternalInputRelayLease,
    ) -> Result<BridgeClaim, String> {
        bridge_handoff::capture(
            &self.shared,
            &ProviderKind::Claude,
            self.channel,
            &self.tmux,
            output,
            lease,
        )
        .await
    }

    async fn resume_unpublished(
        &self,
        shared: &Arc<SharedData>,
    ) -> Option<ExternalInputRelayLease> {
        bridge_handoff::resume_unpublished(shared, &self.row(), &self.source).await
    }

    /// A live supervisor producer on this session; dropping the handle stops it.
    fn producer(&self) -> StreamRelayHandle {
        let relay = crate::services::cluster::stream_relay::spawn_stream_relay(
            crate::services::cluster::session_matcher::MatchedChannel {
                channel_id: self.channel.get().to_string(),
                agent_id: "agent-5808".to_string(),
                provider: ProviderKind::Claude,
                expected_session_name: self.tmux.clone(),
                expected_rollout_path: self.source.to_str().expect("utf8 path").to_string(),
            },
            Arc::new(NullSink),
        );
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
            .register(self.tmux.clone(), relay.producer());
        relay
    }
}

fn refused(wait: NonBridgeWait) -> (Option<InflightEpisodePin>, &'static str) {
    match wait {
        NonBridgeWait::Refused { own_row, reason } => (own_row, reason),
        NonBridgeWait::Yielded => panic!("yielded"),
    }
}

/// The wait yields only when this turn's owner and its live watcher both resolve to the
/// observed source, aliases included; a yielding hand-off leaves the delegated row alone.
#[test]
fn strict_wait_yields_to_an_owner_reading_the_observed_source() {
    isolated(|dir| async move {
        // Watcher row and watcher on B, observed on B or through an alias of B.
        let (b, end) = transcript(&dir, "b.jsonl", "");
        let b_alias = alias(&dir, "b-alias.jsonl", &b);
        let turn = Turn::observe(1, &b, end, Some(&b));
        turn.watch(b.to_str().unwrap());
        assert_eq!(turn.claim().await, ExternalInputRelayOwner::TmuxWatcher);
        for expected in [&b, &b_alias] {
            assert!(matches!(
                turn.wait(expected, turn.turn_id()).await,
                NonBridgeWait::Yielded
            ));
        }
        turn.watch(b_alias.to_str().unwrap());
        assert!(matches!(
            turn.wait(&b, turn.turn_id()).await,
            NonBridgeWait::Yielded
        ));
        let before = turn.row_json();
        assert!(matches!(turn.hand_off(&b).await, NonBridgeHandoff::Yielded));
        assert_eq!(turn.row_json(), before, "a yield writes nothing");
        assert!(delegated_proof_held(&ProviderKind::Claude, turn.channel));

        // Session-bound row on C read by a live watcher on an alias of C, producer live.
        enable_session_bound_delivery();
        let (c, end) = transcript(&dir, "c.jsonl", "");
        let c_alias = alias(&dir, "c-alias.jsonl", &c);
        let turn = Turn::observe(2, &c, end, Some(&c));
        turn.watch(c_alias.to_str().unwrap());
        let _producer = turn.producer();
        assert_eq!(
            turn.claim().await,
            ExternalInputRelayOwner::SessionBoundRelay
        );
        assert!(matches!(
            turn.wait(&c, turn.turn_id()).await,
            NonBridgeWait::Yielded
        ));
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
            .deregister(&turn.tmux);
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
            Turn::observe(n, &path, end, bound.then_some(path.as_path()))
        };
        let source_of = |turn: &Turn| turn.source.to_str().unwrap().to_string();
        let owner_reason = "owner does not read the expected source";

        // Watcher row on B whose live watcher now reads A.
        let turn = case(true);
        turn.watch(&source_of(&turn));
        turn.claim().await;
        turn.watch(a.to_str().unwrap());
        let (own_row, reason) = refused(turn.wait(&turn.source, turn.turn_id()).await);
        assert_eq!((own_row.is_some(), reason), (true, owner_reason));

        // No output on the row (no binding), an empty watcher path, an unresolvable E.
        let turn = case(false);
        turn.watch(&source_of(&turn));
        turn.claim().await;
        assert_eq!(turn.row().output_path, None);
        assert_eq!(
            refused(turn.wait(&turn.source, turn.turn_id()).await).1,
            owner_reason
        );
        let turn = case(true);
        turn.watch(&source_of(&turn));
        turn.claim().await;
        turn.watch("");
        assert_eq!(
            refused(turn.wait(&turn.source, turn.turn_id()).await).1,
            owner_reason
        );
        let missing = dir.join("missing.jsonl");
        let turn = Turn::observe(30, &missing, 0, Some(&missing));
        turn.watch(missing.to_str().unwrap());
        turn.claim().await;
        assert_eq!(turn.row().output_path.as_deref(), missing.to_str());
        assert_eq!(
            refused(turn.wait(&missing, turn.turn_id()).await).1,
            owner_reason
        );

        // A cancelled and a heartbeat-stale watcher on the very same file.
        for stale in [false, true] {
            let turn = case(true);
            let cancel = turn.watch(&source_of(&turn));
            turn.claim().await;
            if stale {
                let watcher = live_watcher(&turn.tmux, &source_of(&turn));
                watcher.last_heartbeat_ts_ms.store(1, Ordering::Release);
                turn.shared.tmux_watchers.insert(turn.channel, watcher);
            } else {
                cancel.store(true, Ordering::Release);
            }
            let (own_row, reason) = refused(turn.wait(&turn.source, turn.turn_id()).await);
            assert_eq!(
                (own_row.is_some(), reason),
                (true, "watcher is not relay-live")
            );
        }

        // Another turn's row on the same source with a live watcher, and a bridge-owned row.
        let turn = case(true);
        turn.watch(&source_of(&turn));
        turn.claim().await;
        let (own_row, reason) = refused(turn.wait(&turn.source, "external:claude:other").await);
        assert_eq!(
            (own_row.is_none(), reason),
            (true, "row belongs to another turn")
        );
        let turn = case(true);
        assert_eq!(turn.claim().await, ExternalInputRelayOwner::BridgeAdapter);
        let (own_row, reason) = refused(turn.wait(&turn.source, turn.turn_id()).await);
        assert_eq!((own_row.is_none(), reason), (true, "row is not delegated"));
    });
}

/// The yield is decided on the row and watcher handle it read: a row whose source changed or
/// that vanished, or a watcher handle replaced, before the recheck cannot yield on that read.
#[test]
fn strict_wait_does_not_yield_on_a_row_or_watcher_that_changed_before_the_recheck() {
    isolated(|dir| async move {
        let (other, _) = transcript(&dir, "other.jsonl", "");
        for (n, change) in [(41, "source"), (42, "removed")] {
            let (b, end) = transcript(&dir, &format!("recheck-{n}.jsonl"), "");
            let turn = Turn::observe(n, &b, end, Some(&b));
            turn.watch(b.to_str().unwrap());
            turn.claim().await;
            let (entered, resume) = arm(&REVALIDATE_PAUSE, turn.channel);
            let act = async {
                entered.notified().await;
                if change == "source" {
                    let mut successor = turn.row();
                    successor.output_path = other.to_str().map(str::to_string);
                    inflight::save_inflight_state(&successor).expect("successor source");
                } else {
                    let row = turn.row();
                    assert_eq!(
                        inflight::clear_inflight_state_if_matches_identity(
                            &ProviderKind::Claude,
                            turn.channel.get(),
                            &inflight::InflightTurnIdentity::from_state(&row),
                        ),
                        inflight::GuardedClearOutcome::Cleared
                    );
                }
                resume.notify_one();
            };
            let (wait, ()) = tokio::join!(turn.wait(&b, turn.turn_id()), act);
            if change == "source" {
                assert_eq!(refused(wait).1, "owner does not read the expected source");
                assert_eq!(
                    turn.row().output_path.as_deref(),
                    other.to_str(),
                    "successor kept"
                );
            } else {
                assert_eq!(refused(wait).1, "no row");
            }
        }

        // The watcher handle is replaced by one on the same file: the read it rechecks is gone.
        let (b, end) = transcript(&dir, "recheck-43.jsonl", "");
        let turn = Turn::observe(43, &b, end, Some(&b));
        turn.watch(b.to_str().unwrap());
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
        assert!(turn.shared.tmux_watchers.remove(&turn.channel).is_some());
        resume_again.notify_one();
        let (own_row, reason) = refused(wait.await);
        assert_eq!(
            (own_row.is_some(), reason),
            (true, "owner does not read the expected source")
        );
    });
}

/// Progress the live turn persists between the read and the recheck does not cost the yield.
#[test]
fn strict_wait_yields_through_progress_on_the_same_episode() {
    isolated(|dir| async move {
        let (b, end) = transcript(&dir, "progress.jsonl", "");
        let turn = Turn::observe(50, &b, end, Some(&b));
        turn.watch(b.to_str().unwrap());
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

/// The P1-1 order: the real wiring adopts a watcher owner and re-records the lease, the watcher
/// dies, and the refused turn is re-owned with a fresh BridgeAdapter lease the old guards keep.
#[test]
fn refused_watcher_turn_is_reowned_for_the_bridge_after_its_watcher_dies() {
    isolated(|dir| async move {
        let (b, end) = transcript(&dir, "wired.jsonl", "");
        let turn = Turn::observe(60, &b, end, Some(&b));
        let cancel = turn.watch(b.to_str().unwrap());
        let hook_lease = turn.wire().await;
        assert_eq!(hook_lease.relay_owner, ExternalInputRelayOwner::TmuxWatcher);
        assert_eq!(turn.live_lease(), Some(hook_lease.clone()));
        cancel.store(true, Ordering::Release);

        let NonBridgeHandoff::Reowned(current) = turn.hand_off(&b).await else {
            panic!("the refused turn is re-owned");
        };
        assert_eq!(current.relay_owner, ExternalInputRelayOwner::BridgeAdapter);
        assert_eq!(current.turn_id, turn.lease.turn_id);
        assert_eq!(
            turn.live_lease(),
            Some(current.clone()),
            "the stored lease is returned"
        );
        let row = turn.row();
        assert_eq!(row.effective_relay_owner_kind(), RelayOwnerKind::None);
        assert_eq!(row.output_path.as_deref(), b.to_str());
        assert_eq!(row.turn_start_offset, Some(0), "same file keeps its start");
        assert!(!delegated_proof_held(&ProviderKind::Claude, turn.channel));

        // Guards still holding the observer's and the wiring's older leases clear nothing now.
        drop(
            super::super::relay_ownership::TuiDirectObservedLeaseEarlyReturnGuard::arm(
                CLAUDE,
                &turn.tmux,
                turn.channel,
                hook_lease.generation,
            ),
        );
        for older in [&turn.lease, &hook_lease] {
            drop(
                super::super::relay_ownership::TuiDirectExternalInputLeaseGuard::new(
                    ProviderKind::Claude,
                    &turn.tmux,
                    turn.channel,
                    older,
                ),
            );
        }
        assert_eq!(turn.live_lease(), Some(current.clone()));
        assert!(
            turn.capture(&b, &current).await.is_ok(),
            "capture takes the re-owned turn"
        );
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
        let turn = Turn::observe(70, &b, end, Some(&a));
        turn.watch(a_alias.to_str().unwrap());
        let _producer = turn.producer();
        assert_eq!(
            turn.claim().await,
            ExternalInputRelayOwner::SessionBoundRelay
        );
        assert_eq!(turn.row().output_path.as_deref(), a.to_str());

        let NonBridgeHandoff::Reowned(current) = turn.hand_off(&b).await else {
            panic!("a session-bound row on another file is re-owned");
        };
        let row = turn.row();
        assert_eq!(row.output_path.as_deref(), b.to_str());
        assert_eq!((row.turn_start_offset, row.last_offset), (Some(end), end));
        assert!(
            std::fs::metadata(&b).unwrap().len() > end,
            "B holds its body past the prompt"
        );
        assert!(turn.capture(&b, &current).await.is_ok());
        crate::services::cluster::relay_producer_registry::global_relay_producer_registry()
            .deregister(&turn.tmux);

        // Watcher row on C observed through an alias of C, its watcher dead.
        let (c, end) = transcript(&dir, "alias-c.jsonl", body);
        let c_alias = alias(&dir, "alias-c-link.jsonl", &c);
        let turn = Turn::observe(71, &c_alias, end, Some(&c));
        let cancel = turn.watch(c.to_str().unwrap());
        turn.claim().await;
        cancel.store(true, Ordering::Release);
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

/// Every proof the re-own rests on, failed one at a time, leaves the row and the lease as they
/// were; the unmodified control re-owns, so each refusal is that proof's.
#[test]
fn reown_leaves_row_and_lease_untouched_when_a_proof_fails() {
    isolated(|dir| async move {
        let not_this_turn = "row is not this turn's undelivered delegation";
        for (n, case) in [
            (80, "control"),
            (81, "watcher reads E at the save"),
            (82, "actor replaced"),
            (83, "placeholder posted"),
            (84, "another turn observed"),
            (85, "body committed"),
            (86, "restarted"),
        ] {
            let body = "{\"type\":\"assistant\"}\n";
            let (b, end) = transcript(&dir, &format!("proof-{n}.jsonl"), body);
            let mut turn = Turn::observe(n, &b, end, Some(&b));
            let cancel = turn.watch(b.to_str().unwrap());
            turn.claim().await;
            cancel.store(true, Ordering::Release);
            let mut _original = None;
            match case {
                "actor replaced" | "restarted" => {
                    let snapshot =
                        crate::services::discord::mailbox_snapshot(&turn.shared, turn.channel)
                            .await;
                    let actor = snapshot.cancel_token.expect("claimed actor");
                    crate::services::discord::mailbox_finish_turn(
                        &turn.shared,
                        &ProviderKind::Claude,
                        turn.channel,
                    )
                    .await;
                    if case == "actor replaced" {
                        let successor = Arc::new(CancelToken::from_persisted_turn_nonce(
                            actor.turn_nonce().map(str::to_owned),
                        ));
                        assert!(
                            crate::services::discord::mailbox_try_start_turn(
                                &turn.shared,
                                turn.channel,
                                successor,
                                serenity::UserId::new(TUI_DIRECT_SYNTHETIC_OWNER_USER_ID),
                                turn.anchor(),
                            )
                            .await
                        );
                        _original = Some(actor);
                    }
                }
                "placeholder posted" => {
                    let mut posted = turn.row();
                    posted.current_msg_id = turn.anchor().get() + 3;
                    inflight::save_inflight_state(&posted).expect("placeholder");
                }
                "another turn observed" => {
                    turn.lease.turn_id = Some("external:claude:another-observation".to_string());
                }
                "body committed" => {
                    let eof = std::fs::metadata(&b).unwrap().len();
                    (turn
                        .shared
                        .tmux_relay_coord(turn.channel)
                        .confirmed_end_offset)
                        .store(eof, Ordering::Release);
                }
                _ => {}
            }
            let (row, lease) = (turn.row_json(), turn.live_lease());
            let result = if case == "watcher reads E at the save" {
                let (entered, resume) = arm(&REOWN_PAUSE, turn.channel);
                let act = async {
                    entered.notified().await;
                    turn.watch(b.to_str().unwrap());
                    resume.notify_one();
                };
                tokio::join!(turn.reown(), act).0
            } else {
                turn.reown().await
            };
            let expected = match case {
                "control" => {
                    assert!(result.is_ok(), "control re-owns: {result:?}");
                    continue;
                }
                "watcher reads E at the save" => "a live watcher may read the observed source",
                "actor replaced" => "delegated actor is not the current mailbox turn",
                "body committed" => "committed frontier covers the turn body",
                "restarted" => "delegated actor is gone",
                _ => not_this_turn,
            };
            assert_eq!(result.err(), Some(expected), "{case}");
            assert_eq!(turn.row_json(), row, "{case}: row untouched");
            assert_eq!(turn.live_lease(), lease, "{case}: lease untouched");
        }
    });
}

/// A re-owned turn whose tail fails, before or after capture stamped a native session on the
/// row, resumes through the dormant consumer on the witness the re-own left behind.
#[test]
fn reowned_turn_resumes_through_the_dormant_consumer_after_a_failed_tail() {
    isolated(|dir| async move {
        let (b, end) = transcript(&dir, "dormant.jsonl", "");
        let turn = Turn::observe(90, &b, end, Some(&b));
        let cancel = turn.watch(b.to_str().unwrap());
        turn.wire().await;
        cancel.store(true, Ordering::Release);
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

/// Rows from an older build spell a watcher owner as the legacy flag: the re-own takes it and
/// clears the flag, and after a restart the re-owned row resumes with no in-memory proof left.
#[test]
fn reown_takes_a_legacy_watcher_row_and_the_result_resumes_after_restart() {
    isolated(|dir| async move {
        let (b, end) = transcript(&dir, "legacy.jsonl", "");
        let turn = Turn::observe(100, &b, end, Some(&b));
        let cancel = turn.watch(b.to_str().unwrap());
        turn.claim().await;
        cancel.store(true, Ordering::Release);
        let mut legacy = turn.row();
        legacy.relay_owner_kind = RelayOwnerKind::None;
        legacy.watcher_owns_live_relay = true;
        inflight::save_inflight_state(&legacy).expect("legacy row");
        assert_eq!(
            turn.row().effective_relay_owner_kind(),
            RelayOwnerKind::Watcher
        );

        let current = turn.reown().await.expect("legacy watcher row is re-owned");
        let row = turn.row();
        assert_eq!(
            (row.relay_owner_kind, row.watcher_owns_live_relay),
            (RelayOwnerKind::None, false)
        );
        drop(turn.capture(&b, &current).await.expect("capture"));

        crate::services::discord::mailbox_finish_turn(
            &turn.shared,
            &ProviderKind::Claude,
            turn.channel,
        )
        .await;
        let restarted = crate::services::discord::make_shared_data_for_tests();
        assert!(turn.live_lease().is_none(), "no lease survives the restart");
        assert!(turn.resume_unpublished(&restarted).await.is_some());
    });
}

/// With S0 alone a delegated claim decides exactly as before: the existing wait yields to it,
/// a re-claim cannot take it as the bridge, and neither capture nor dormant resume accepts it.
#[test]
fn a_delegated_claim_leaves_yield_reclaim_capture_and_dormant_as_before() {
    isolated(|dir| async move {
        let (b, end) = transcript(&dir, "dormant-equal.jsonl", "");
        let turn = Turn::observe(110, &b, end, Some(&b));
        let cancel = turn.watch(b.to_str().unwrap());
        turn.wire().await;
        cancel.store(true, Ordering::Release);
        let before = turn.row_json();

        assert!(
            synthetic_start::wait_for_tui_direct_synthetic_non_bridge_claim(
                &turn.shared.tmux_watchers,
                &ProviderKind::Claude,
                turn.channel,
                &turn.tmux,
            )
            .await
        );
        let again = synthetic_start::claim_tui_direct_synthetic_turn(
            &turn.shared,
            &ProviderKind::Claude,
            turn.channel,
            &turn.tmux,
            PROMPT,
            turn.anchor(),
            &turn.lease,
        )
        .await;
        assert!(
            !again.claimed,
            "a re-claim cannot take the delegated row as the bridge"
        );
        assert!(turn.resume_unpublished(&turn.shared).await.is_none());
        tokio::time::pause();
        assert!(turn.capture(&b, &turn.lease).await.is_err());
        tokio::time::resume();
        assert_eq!(turn.row_json(), before);
    });
}

/// The delegated proof is one entry per channel, and an entry whose actor is gone is dropped at
/// the next delegation instead of accumulating.
#[test]
fn delegated_actor_proofs_do_not_accumulate() {
    isolated(|dir| async move {
        let delegate = |n: u64| {
            let (b, end) = transcript(&dir, &format!("bounded-{n}.jsonl"), "");
            let turn = Turn::observe(n, &b, end, Some(&b));
            turn.watch(b.to_str().unwrap());
            turn
        };
        let first = delegate(120);
        first.claim().await;
        assert!(delegated_proof_held(&ProviderKind::Claude, first.channel));
        crate::services::discord::mailbox_finish_turn(
            &first.shared,
            &ProviderKind::Claude,
            first.channel,
        )
        .await;
        assert!(delegated_proof_held(&ProviderKind::Claude, first.channel));
        let second = delegate(121);
        second.claim().await;
        assert!(!delegated_proof_held(&ProviderKind::Claude, first.channel));
        assert!(delegated_proof_held(&ProviderKind::Claude, second.channel));
    });
}
