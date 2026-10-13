//! The refused-delegation re-own and the delegated actor proof, on rows the real claim made.
use super::super::super::non_bridge_claim::{
    NonBridgeHandoff, NonBridgeWait, hand_off_observed_turn, wait_for_source_bound_non_bridge_claim,
};
use super::*;
use crate::services::cluster::stream_relay::{
    RelaySink, RelaySinkError, RelaySinkOutcome, StreamFrame, StreamRelayHandle,
};
use crate::services::discord::session_relay_sink::tests::idle_relay_harness::live_watcher;
use crate::services::discord::{SharedData, inflight};
use std::future::Future;
use tokio::sync::Notify;

pub(in crate::services::discord::tui_prompt_relay::synthetic_start) const PROMPT: &str =
    "please review the 5808 handoff";
const CLAUDE: &str = "claude";

/// Runs `body` with the dedupe maps to itself, under its own runtime root.
pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn isolated<
    F: Future<Output = ()>,
>(
    body: impl FnOnce(PathBuf) -> F,
) {
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
    let slots: [&PauseSlot; 2] = [
        &super::super::super::non_bridge_claim::REVALIDATE_PAUSE,
        &REOWN_PAUSE,
    ];
    for slot in slots {
        *slot.lock().unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}

/// A transcript holding the observed prompt, then `body`; returns it and the prompt end.
pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn transcript(
    dir: &Path,
    name: &str,
    body: &str,
) -> (PathBuf, u64) {
    let path = dir.join(name);
    let prompt = format!(
        "{}\n",
        serde_json::json!({"type": "user", "message": {"content": PROMPT}})
    );
    std::fs::write(&path, format!("{prompt}{body}")).expect("transcript");
    (path, prompt.len() as u64)
}

pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn alias(
    dir: &Path,
    name: &str,
    target: &Path,
) -> PathBuf {
    let path = dir.join(name);
    std::os::unix::fs::symlink(target, &path).expect("alias");
    path
}

pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn arm(
    slot: &'static PauseSlot,
    channel: ChannelId,
) -> (Arc<Notify>, Arc<Notify>) {
    let (entered, resume) = (Arc::new(Notify::new()), Arc::new(Notify::new()));
    *slot.lock().unwrap_or_else(|poison| poison.into_inner()) =
        Some((channel.get(), entered.clone(), resume.clone()));
    (entered, resume)
}

pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn enable_session_bound_delivery()
 {
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
pub(in crate::services::discord::tui_prompt_relay::synthetic_start) struct Turn {
    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) shared: Arc<SharedData>,
    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) channel: ChannelId,
    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) tmux: String,
    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) source: PathBuf,
    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) prompt_end: u64,
    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) lease:
        ExternalInputRelayLease,
}

impl Turn {
    /// Binds `bound` as the session transcript, starts a watcher on `watched`, and records the
    /// observer's lease for `source` the way the idle caller does.
    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn observe(
        n: u64,
        source: &Path,
        prompt_end: u64,
        bound: Option<&Path>,
        watched: Option<&Path>,
    ) -> Self {
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
        let turn = Self {
            shared,
            channel,
            tmux,
            source: source.to_path_buf(),
            prompt_end,
            lease: ExternalInputRelayLease::unassigned(Some(channel.get())),
        };
        if let Some(watched) = watched {
            turn.watch(watched.to_str().expect("utf8 path"));
        }
        let lease =
            super::super::super::super::relay_ownership::record_external_turn_lease_for_output(
                &turn.shared,
                &ProviderKind::Claude,
                channel,
                &turn.tmux,
                RuntimeHandoffKind::ClaudeTui,
                source,
                chrono::Utc::now(),
            );
        Self { lease, ..turn }
    }

    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn anchor(
        &self,
    ) -> MessageId {
        MessageId::new(self.channel.get() + 1)
    }

    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn turn_id(&self) -> &str {
        self.lease.turn_id.as_deref().expect("observed turn id")
    }

    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn watch(
        &self,
        path: &str,
    ) {
        self.shared
            .tmux_watchers
            .insert(self.channel, live_watcher(&self.tmux, path));
    }

    /// The watcher stops heartbeating, as a dead one does, and stays registered.
    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn stop_watcher(&self) {
        let watcher = self.shared.tmux_watchers.by_tmux_session.get(&self.tmux);
        let watcher = watcher.expect("registered watcher");
        watcher.last_heartbeat_ts_ms.store(1, Ordering::Release);
    }

    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) async fn claim(
        &self,
    ) -> ExternalInputRelayOwner {
        let claim = super::super::super::claim_tui_direct_synthetic_turn(
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
    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) async fn wire(
        &self,
    ) -> ExternalInputRelayLease {
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
        let decision = super::super::super::super::observed_prompt_decision::relay_observed_prompt_injected_prompt_decision(PROMPT);
        let mut hook_lease = self.lease.clone();
        let deferred =
            super::super::super::super::synthetic_start_wiring::wire_tui_direct_synthetic_turn_start(
                &self.shared,
                CLAUDE,
                self.channel,
                &prompt,
                self.anchor(),
                false,
                &decision,
                &mut hook_lease,
            )
            .await;
        assert!(!deferred, "no prior turn, so the claim runs inline");
        hook_lease
    }

    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn row(
        &self,
    ) -> InflightTurnState {
        inflight::load_inflight_state_read_only(&ProviderKind::Claude, self.channel.get())
            .expect("claimed row")
    }

    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn row_json(
        &self,
    ) -> serde_json::Value {
        serde_json::to_value(self.row()).expect("row json")
    }

    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn live_lease(
        &self,
    ) -> Option<ExternalInputRelayLease> {
        crate::services::tui_prompt_dedupe::external_input_relay_lease(
            CLAUDE,
            &self.tmux,
            self.channel.get(),
        )
    }

    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) async fn wait(
        &self,
        expected: &Path,
        turn_id: &str,
    ) -> NonBridgeWait {
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

    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) async fn hand_off(
        &self,
        expected: &Path,
    ) -> NonBridgeHandoff {
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

    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) async fn capture(
        &self,
        output: &Path,
        lease: &ExternalInputRelayLease,
    ) -> Result<BridgeClaim, String> {
        capture(
            &self.shared,
            &ProviderKind::Claude,
            self.channel,
            &self.tmux,
            output,
            lease,
        )
        .await
    }

    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) async fn resume_unpublished(
        &self,
        shared: &Arc<SharedData>,
    ) -> Option<ExternalInputRelayLease> {
        resume_unpublished(shared, &self.row(), &self.source).await
    }

    /// A live supervisor producer on this session; dropping the handle stops it.
    pub(in crate::services::discord::tui_prompt_relay::synthetic_start) fn producer(
        &self,
    ) -> StreamRelayHandle {
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
            let (b, end) = transcript(
                &dir,
                &format!("proof-{n}.jsonl"),
                "{\"type\":\"assistant\"}\n",
            );
            let mut turn = Turn::observe(n, &b, end, Some(&b), Some(&b));
            turn.claim().await;
            turn.stop_watcher();
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
                    let eof = std::fs::metadata(&b).expect("transcript").len();
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
                let reown = turn.reown();
                tokio::pin!(reown);
                tokio::select! {
                    _ = entered.notified() => {}
                    result = &mut reown => panic!("{case}: the re-own reaches the save: {result:?}"),
                }
                turn.watch(b.to_str().expect("utf8 path"));
                resume.notify_one();
                reown.await
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

/// Rows from an older build spell a watcher owner as the legacy flag: the re-own takes it and
/// clears the flag, and after a restart the re-owned row resumes with no in-memory proof left.
#[test]
fn reown_takes_a_legacy_watcher_row_and_the_result_resumes_after_restart() {
    isolated(|dir| async move {
        let (b, end) = transcript(&dir, "legacy.jsonl", "");
        let turn = Turn::observe(100, &b, end, Some(&b), Some(&b));
        turn.claim().await;
        turn.stop_watcher();
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
        let turn = Turn::observe(110, &b, end, Some(&b), Some(&b));
        turn.wire().await;
        turn.stop_watcher();
        let before = turn.row_json();

        assert!(
            super::super::super::wait_for_tui_direct_synthetic_non_bridge_claim(
                &turn.shared.tmux_watchers,
                &ProviderKind::Claude,
                turn.channel,
                &turn.tmux,
            )
            .await
        );
        let again = super::super::super::claim_tui_direct_synthetic_turn(
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
            Turn::observe(n, &b, end, Some(&b), Some(&b))
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
