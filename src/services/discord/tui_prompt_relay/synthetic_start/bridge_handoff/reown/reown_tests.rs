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
    let slots: [&PauseSlot; 4] = [
        &super::super::super::non_bridge_claim::REVALIDATE_PAUSE,
        &super::super::super::non_bridge_claim::HANDOFF_PAUSE,
        &super::super::super::super::synthetic_start_wiring::ADOPT_PAUSE,
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

    fn row_path(&self) -> PathBuf {
        let root = inflight::inflight_runtime_root().expect("runtime root");
        inflight::inflight_state_path(&root, &ProviderKind::Claude, self.channel.get())
    }

    /// The row file as stored, then whether the bridge and the delegated proof maps hold the
    /// channel: everything but the lease that a refused re-own must leave alone.
    fn stored(&self) -> (Vec<u8>, bool, bool) {
        let key = (CLAUDE.to_owned(), self.channel.get());
        let claimed =
            (CLAIMS.lock().unwrap_or_else(|poison| poison.into_inner())).contains_key(&key);
        (
            std::fs::read(self.row_path()).expect("row file"),
            claimed,
            delegated_proof_held(&ProviderKind::Claude, self.channel),
        )
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

/// A delegated claim stays fenced until it is re-owned: a re-claim cannot take it as the
/// bridge, and neither capture nor dormant resume accepts it.
#[test]
fn a_delegated_claim_is_refused_by_reclaim_capture_and_dormant() {
    isolated(|dir| async move {
        let (b, end) = transcript(&dir, "dormant-equal.jsonl", "");
        let turn = Turn::observe(110, &b, end, Some(&b), Some(&b));
        turn.wire().await;
        turn.stop_watcher();
        let before = turn.row_json();

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

/// Parks `turn`'s re-own just before its commit and returns it with the resume signal.
macro_rules! parked_reown {
    ($turn:expr, $reown:ident, $resume:ident) => {
        let (entered, $resume) = arm(&REOWN_PAUSE, $turn.channel);
        let $reown = $turn.reown();
        tokio::pin!($reown);
        tokio::select! {
            _ = entered.notified() => {}
            result = &mut $reown => panic!("the re-own reaches its commit: {result:?}"),
        }
    };
}

/// The lease the re-own read is replaced or cleared before its commit, each through the entry
/// that does so in production: nothing is committed and the successor, or no lease, stands.
#[test]
fn reown_commits_nothing_when_the_stored_lease_changes_before_the_commit() {
    isolated(|dir| async move {
        for (n, case) in [
            (140, "control"),
            (141, "next turn observed"),
            (142, "same turn re-recorded"),
            (143, "cleared by its tail guard"),
            (144, "cleared by its hook guard"),
            (145, "cleared outright"),
        ] {
            let (b, end) = transcript(&dir, &format!("lease-{n}.jsonl"), "");
            // Only a bridge-owned lease reaches the tail guard's exact clear: observe with no
            // watcher, then let the claim delegate to one without the wiring's adopt.
            let bridge_leased = case == "cleared by its tail guard";
            let turn = Turn::observe(n, &b, end, Some(&b), (!bridge_leased).then_some(&*b));
            if bridge_leased {
                turn.watch(b.to_str().expect("utf8 path"));
                turn.claim().await;
            } else {
                turn.wire().await;
            }
            turn.stop_watcher();
            let stored = turn.live_lease().expect("stored lease");
            assert_eq!(
                stored.turn_id, turn.lease.turn_id,
                "{case}: this turn's lease"
            );
            let before = turn.stored();
            parked_reown!(turn, reown, resume);
            let successor = match case {
                "control" => Some(stored.clone()),
                "next turn observed" => Some(
                    super::super::super::super::relay_ownership::record_external_turn_lease_for_output(
                        &turn.shared,
                        &ProviderKind::Claude,
                        turn.channel,
                        &turn.tmux,
                        RuntimeHandoffKind::ClaudeTui,
                        &b,
                        chrono::Utc::now() + chrono::Duration::seconds(1),
                    ),
                ),
                "same turn re-recorded" => Some(
                    crate::services::tui_prompt_dedupe::record_external_input_turn_lease(
                        CLAUDE,
                        &turn.tmux,
                        stored.clone(),
                    ),
                ),
                "cleared by its tail guard" => {
                    drop(TuiDirectExternalInputLeaseGuard::new(
                        ProviderKind::Claude,
                        &turn.tmux,
                        turn.channel,
                        &stored,
                    ));
                    None
                }
                "cleared by its hook guard" => {
                    drop(
                        super::super::super::super::relay_ownership::TuiDirectObservedLeaseEarlyReturnGuard::arm(
                            CLAUDE,
                            &turn.tmux,
                            turn.channel,
                            stored.generation,
                        ),
                    );
                    None
                }
                _ => {
                    crate::services::tui_prompt_dedupe::clear_external_input_relay_lease(
                        CLAUDE,
                        &turn.tmux,
                        turn.channel.get(),
                    );
                    None
                }
            };
            assert_eq!(
                turn.live_lease(),
                successor,
                "{case}: the entry took effect"
            );
            resume.notify_one();
            let result = reown.await;
            if case == "control" {
                assert!(result.is_ok(), "control re-owns: {result:?}");
                continue;
            }
            let changed = "live lease changed before the re-own committed";
            assert_eq!(result.err(), Some(changed), "{case}");
            assert_eq!(
                turn.live_lease(),
                successor,
                "{case}: successor lease stands"
            );
            assert_eq!(
                turn.stored(),
                before,
                "{case}: row bytes and proofs untouched"
            );
        }
    });
}

/// A re-own whose row save fails, or whose bridge witness is refused after the save, keeps the
/// lease it read and the delegated proof, and leaves the row delegated.
#[test]
fn reown_keeps_the_lease_and_the_delegation_when_its_commit_fails() {
    use std::os::unix::fs::PermissionsExt;
    isolated(|dir| async move {
        for (n, case, expected) in [
            (160, "row save fails", "re-owned row was not saved"),
            (161, "witness refused", "bridge witness was refused"),
        ] {
            let (b, end) = transcript(&dir, &format!("commit-{n}.jsonl"), "");
            let turn = Turn::observe(n, &b, end, Some(&b), Some(&b));
            let stored = turn.wire().await;
            turn.stop_watcher();
            let (before, before_row) = (turn.stored(), turn.row());
            parked_reown!(turn, reown, resume);
            let row_dir = turn.row_path().parent().expect("row dir").to_path_buf();
            let mut _rival = None;
            if case == "row save fails" {
                std::fs::set_permissions(&row_dir, std::fs::Permissions::from_mode(0o555))
                    .expect("read-only row dir");
            } else {
                // Another live allocation already witnesses the row the re-own is about to save.
                let mut ownerless = turn.row();
                ownerless.set_relay_owner_kind(RelayOwnerKind::None);
                let rival = Arc::new(CancelToken::from_persisted_turn_nonce(
                    ownerless.turn_nonce.clone(),
                ));
                assert!(record(&ownerless, Some(&rival), None), "rival witness");
                _rival = Some(rival);
            }
            resume.notify_one();
            let result = reown.await;
            std::fs::set_permissions(&row_dir, std::fs::Permissions::from_mode(0o755))
                .expect("writable row dir");
            assert_eq!(result.err(), Some(expected), "{case}");
            assert_eq!(turn.live_lease(), Some(stored), "{case}: the lease as read");
            assert!(delegated_proof_held(&ProviderKind::Claude, turn.channel));
            let mut row = turn.row();
            assert_eq!(
                row.effective_relay_owner_kind(),
                RelayOwnerKind::Watcher,
                "{case}"
            );
            if case == "row save fails" {
                assert_eq!(turn.stored(), before, "{case}: nothing was written");
            } else {
                // The restore is a write of its own; only its save bookkeeping differs.
                row.save_generation = before_row.save_generation;
                row.updated_at = before_row.updated_at.clone();
                assert_eq!(
                    serde_json::to_value(&row).expect("row json"),
                    serde_json::to_value(&before_row).expect("row json"),
                    "{case}"
                );
            }
        }
    });
}

/// A stopped watcher that could read `E` may still be mid-send. The re-own takes the turn only
/// once that watcher's task has ended, and then judges the row the task left behind.
#[test]
fn reown_waits_for_a_stopped_watcher_task_to_end() {
    isolated(|dir| async move {
        for (n, case) in [
            (150, "still sending"),
            (151, "delivered, then ended"),
            (152, "ended without sending"),
        ] {
            let (b, end) = transcript(&dir, &format!("stopped-{n}.jsonl"), "");
            let turn = Turn::observe(n, &b, end, Some(&b), Some(&b));
            let stored = turn.wire().await;
            let watcher = turn.shared.tmux_watchers.by_tmux_session.get(&turn.tmux);
            let cancel = watcher.expect("registered watcher").cancel.clone();
            // The watcher's supervised task outlives its cancel until `finish` lets it return.
            let finish = Arc::new(Notify::new());
            let (released, channel, delivers) = (
                finish.clone(),
                turn.channel,
                case == "delivered, then ended",
            );
            let task = crate::services::discord::task_supervisor::spawn_observed_tmux_watcher(
                "test_5808_stopped_watcher",
                turn.shared.clone(),
                turn.tmux.clone(),
                cancel.clone(),
                async move {
                    released.notified().await;
                    if delivers {
                        let mut sent = inflight::load_inflight_state_read_only(
                            &ProviderKind::Claude,
                            channel.get(),
                        )
                        .expect("delegated row");
                        sent.last_watcher_relayed_offset = Some(1);
                        inflight::save_inflight_state(&sent).expect("watcher commit");
                    }
                },
            );
            cancel.store(true, Ordering::Release);
            let before = turn.stored();
            if case == "still sending" {
                tokio::time::pause();
                let result = turn.reown().await;
                tokio::time::resume();
                assert_eq!(result.err(), Some(SEND_UNKNOWN), "{case}");
                assert_eq!(
                    turn.stored(),
                    before,
                    "{case}: row bytes and proofs untouched"
                );
                assert_eq!(turn.live_lease(), Some(stored), "{case}: lease untouched");
                finish.notify_one();
                task.await.expect("watcher task");
                continue;
            }
            finish.notify_one();
            let result = turn.reown().await;
            task.await.expect("watcher task");
            if delivers {
                let delivered = "row is not this turn's undelivered delegation";
                assert_eq!(result.err(), Some(delivered), "{case}");
                assert_eq!(turn.live_lease(), Some(stored), "{case}: lease untouched");
            } else {
                assert!(result.is_ok(), "{case}: {result:?}");
            }
        }
    });
}
