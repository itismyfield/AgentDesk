use super::*;
use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
use crate::services::discord;
use crate::services::discord::inflight::RelayOwnerKind;
use crate::services::provider::ProviderKind;
use crate::services::provider::cancel_token_claude_interrupt::{
    HERDR_SETTLEMENT_OVERRIDE, HerdrSubmission,
};
use crate::services::provider::herdr_before_start::seal_exit;
use crate::services::provider::herdr_before_start::{
    InputPhase, finish_execution, prelaunch_closed,
};
use poise::serenity_prelude::UserId;
use std::sync::atomic::Ordering;

fn prepared(provider: ProviderKind, channel: u64) -> (Arc<CancelToken>, HostedOwner) {
    let actor = Arc::new(CancelToken::new());
    let owner = HostedOwner {
        provider: provider.as_str().into(),
        discord_token_hash: "test-token-hash".into(),
        channel_id: channel.to_string(),
        logical_key: format!("AgentDesk-{}-cold-{channel}", provider.as_str()),
        owner_node: "test-node".into(),
        runtime_root: "test-root".into(),
    };
    actor.prepare_herdr_interrupt(provider, &owner);
    (actor, owner)
}

#[test]
fn coldstop_phases_require_positive_untouched_and_preserve_base_hold() {
    HERDR_SETTLEMENT_OVERRIDE.set(true);
    for provider in [ProviderKind::Claude, ProviderKind::Codex] {
        for phase in [
            InputPhase::FinishedNoAttempt,
            InputPhase::Attempting,
            InputPhase::FinishedUntouched,
            InputPhase::MayHaveWritten,
        ] {
            let (actor, _) = prepared(provider.clone(), 901);
            let state = actor.herdr_interrupt_state().unwrap();
            state.submission.lock().unwrap().phase = phase;
            state.user_stop.store(true, Ordering::Release);
            let decision = seal_exit(&actor, false, false);
            assert_eq!(
                matches!(decision, ExitDecision::PolicyClose(_)),
                matches!(
                    phase,
                    InputPhase::FinishedNoAttempt | InputPhase::FinishedUntouched
                )
            );
            // The bridge exit reads the sealed decision, never a fresh stop observation.
            assert_eq!(
                discord::turn_bridge::stream_loop::exit_reconcile::herdr_exit_decision(
                    &actor, false, false
                ),
                decision
            );
        }
        // A submission observation (an Escape interrupted after its send, before its ACK)
        // holds even where the phase alone would have been negative evidence.
        for phase in [
            InputPhase::BeforeInput,
            InputPhase::FinishedNoAttempt,
            InputPhase::FinishedUntouched,
        ] {
            for submission in [HerdrSubmission::Submitted, HerdrSubmission::Unknown] {
                let (actor, _) = prepared(provider.clone(), 902);
                let state = actor.herdr_interrupt_state().unwrap();
                let mut input = state.submission.lock().unwrap();
                input.submission = submission;
                input.phase = phase;
                drop(input);
                state.user_stop.store(true, Ordering::Release);
                assert_eq!(seal_exit(&actor, false, false), ExitDecision::Hold);
            }
        }
        let (actor, _) = prepared(provider.clone(), 903);
        finish_execution(Some(&actor));
        assert_eq!(
            actor
                .herdr_interrupt_state()
                .unwrap()
                .submission
                .lock()
                .unwrap()
                .phase,
            InputPhase::FinishedNoAttempt
        );
        actor
            .herdr_interrupt_state()
            .unwrap()
            .user_stop
            .store(true, Ordering::Release);
        assert!(matches!(
            seal_exit(&actor, false, false),
            ExitDecision::PolicyClose(_)
        ));
    }
}

#[test]
fn coldstop_off_keeps_phase_submission_and_intent_bytes() {
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    let (actor, _) = prepared(ProviderKind::Codex, 904);
    let state = actor.herdr_interrupt_state().unwrap();
    state.user_stop.store(true, Ordering::Release);
    assert!(!prelaunch_closed(Some(&actor)).unwrap());
    assert_eq!(
        state.submission.lock().unwrap().phase,
        InputPhase::BeforeInput
    );
    assert_eq!(seal_exit(&actor, false, false), ExitDecision::Normal);
    assert_eq!(
        discord::turn_bridge::stream_loop::exit_reconcile::herdr_exit_decision(
            &actor, false, false
        ),
        ExitDecision::Normal
    );
    HERDR_SETTLEMENT_OVERRIDE.set(true);
}

#[test]
fn coldstop_typed_untouched_not_indeterminate_and_late_exit_is_sealed() {
    for untouched in [false, true] {
        let (actor, _) = prepared(ProviderKind::Codex, 905);
        let state = actor.herdr_interrupt_state().unwrap();
        let mut input = state.submission.lock().unwrap();
        assert!(state.prepare_input(&mut input));
        state.finish_input(&mut input, HerdrSubmission::Unsubmitted, untouched);
        drop(input);
        state.user_stop.store(true, Ordering::Release);
        assert_eq!(
            matches!(
                seal_exit(&actor, false, false),
                ExitDecision::PolicyClose(_)
            ),
            untouched
        );
    }
    let (actor, _) = prepared(ProviderKind::Claude, 906);
    assert_eq!(seal_exit(&actor, false, false), ExitDecision::Normal);
    actor
        .herdr_interrupt_state()
        .unwrap()
        .user_stop
        .store(true, Ordering::Release);
    assert_eq!(seal_exit(&actor, false, false), ExitDecision::Normal);
}

#[tokio::test(flavor = "current_thread")]
async fn coldstop_mailbox_writer_and_snapshot_do_not_wait_for_input_lock() {
    let shared = discord::make_shared_data_for_tests_with_storage(None);
    let (actor, _) = prepared(shared.provider.clone(), 912);
    let channel = ChannelId::new(912);
    assert!(
        discord::mailbox_try_start_turn_kinded(
            &shared,
            channel,
            actor.clone(),
            UserId::new(1),
            MessageId::new(912),
            crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
        )
        .await
    );
    let state = actor.herdr_interrupt_state().unwrap();
    let (locked_tx, locked_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let input_state = state.clone();
    let input_thread = std::thread::spawn(move || {
        let _input = input_state.submission.lock().unwrap();
        locked_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    locked_rx.recv().unwrap();
    assert_eq!(state.closed_probe(), None);
    assert_eq!(seal_exit(&actor, false, false), ExitDecision::Hold);
    let mailbox = shared.mailbox_peek(channel).unwrap();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let runtime = tokio::runtime::Handle::current();
    let stop_actor = actor.clone();
    let stop_state = state.clone();
    let worker = std::thread::spawn(move || {
        // The mutation simulates a forbidden lock in the actual writer's request path.
        let _blocking = crate::services::provider::herdr_before_start::mutant(
            "stop_writer_blocks_on_submission",
        )
        .then(|| stop_state.submission.lock().unwrap());
        let accepted = runtime
            .block_on(mailbox.admit_herdr_user_stop_if_current(stop_actor, "cold stop".into()));
        let snapshot = runtime.block_on(mailbox.snapshot());
        done_tx
            .send((accepted.token.is_some(), snapshot.cancel_token.is_some()))
            .unwrap();
    });
    let started = std::time::Instant::now();
    let mut result = None;
    while started.elapsed() < std::time::Duration::from_millis(500) {
        if let Ok(done) = done_rx.try_recv() {
            result = Some(done);
            break;
        }
        tokio::task::yield_now().await;
    }
    release_tx.send(()).unwrap();
    input_thread.join().unwrap();
    if result.is_none() {
        while !worker.is_finished() {
            tokio::task::yield_now().await;
        }
    }
    worker.join().unwrap();
    assert_eq!(
        result,
        Some((true, true)),
        "stop/snapshot must return while input lock is held"
    );
    assert!(state.user_stop.load(Ordering::Acquire));
}

#[test]
fn coldstop_exit_exceptions_precede_busy_input_probe() {
    let (actor, _) = prepared(ProviderKind::Claude, 916);
    let state = actor.herdr_interrupt_state().unwrap();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        let _lock = state.submission.lock().unwrap();
        ready_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    ready_rx.recv().unwrap();
    HERDR_SETTLEMENT_OVERRIDE.set(false);
    let off = seal_exit(&actor, false, false);
    HERDR_SETTLEMENT_OVERRIDE.set(true);
    let cancelled = seal_exit(&actor, true, false);
    let admitted = seal_exit(&actor, false, true);
    let normal_hold = seal_exit(&actor, false, false);
    release_tx.send(()).unwrap();
    thread.join().unwrap();
    assert_eq!(
        (off, cancelled, admitted),
        (
            ExitDecision::Normal,
            ExitDecision::Normal,
            ExitDecision::Normal
        )
    );
    assert_eq!(normal_hold, ExitDecision::Hold);
}

#[test]
fn coldstop_exit_seal_reads_one_stop_observation() {
    use crate::services::provider::herdr_before_start::EXIT_OBSERVED;
    HERDR_SETTLEMENT_OVERRIDE.set(true);
    let stop_now =
        |state: &crate::services::provider::cancel_token_claude_interrupt::HerdrInterruptState| {
            state.user_stop.store(true, Ordering::Release);
        };
    let mut sealed = Vec::new();
    for (channel, submission, early) in [
        (917, HerdrSubmission::Unsubmitted, true),
        (918, HerdrSubmission::Unsubmitted, false),
        (919, HerdrSubmission::Submitted, false),
    ] {
        let (actor, _) = prepared(ProviderKind::Codex, channel);
        let state = actor.herdr_interrupt_state().unwrap();
        let mut input = state.submission.lock().unwrap();
        input.phase = InputPhase::FinishedNoAttempt;
        input.submission = submission;
        drop(input);
        state.user_stop.store(early, Ordering::Release);
        // A stop the writer lands after the seal's observation, inside the seal.
        EXIT_OBSERVED.set(Some(stop_now));
        let decision = seal_exit(&actor, false, false);
        EXIT_OBSERVED.set(None);
        assert!(state.user_stop.load(Ordering::Acquire));
        // The late stop does not reopen a sealed decision.
        assert_eq!(seal_exit(&actor, false, false), decision);
        sealed.push(match decision {
            ExitDecision::PolicyClose(_) => "close",
            ExitDecision::Hold => "hold",
            ExitDecision::Normal => "normal",
        });
    }
    assert_eq!(sealed, ["close", "normal", "hold"]);
}

#[test]
fn coldstop_closed_drops_callback_and_own_start_is_negative_evidence() {
    let (actor, _) = prepared(ProviderKind::Claude, 914);
    let state = actor.herdr_interrupt_state().unwrap();
    let fired = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let callback_fired = fired.clone();
    assert!(state.arm_late_stop(Box::new(move || {
        callback_fired.fetch_add(1, Ordering::Relaxed);
    })));
    state.user_stop.store(true, Ordering::Release);
    assert!(prelaunch_closed(Some(&actor)).unwrap());
    state.own_start_observed(1, "later-start");
    assert_eq!(fired.load(Ordering::Relaxed), 0);
    let (actor, _) = prepared(ProviderKind::Codex, 915);
    let state = actor.herdr_interrupt_state().unwrap();
    state.own_start_observed(1, "own");
    finish_execution(Some(&actor));
    state.user_stop.store(true, Ordering::Release);
    assert_eq!(seal_exit(&actor, false, false), ExitDecision::Hold);
}

/// A notice Discord showed by editing the turn's message.
async fn shown() -> Result<ReplaceLongMessageOutcome, String> {
    Ok(ReplaceLongMessageOutcome::EditedOriginal)
}

pub(in crate::services::discord::turn_bridge) struct NoLaunch;
impl crate::services::claude::herdr_turn::HerdrTurnPorts for NoLaunch {
    fn launch_host(&self) -> Option<Arc<dyn crate::services::herdr_launch::HerdrLaunchHost>> {
        None
    }
    fn hook_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::services::claude_tui::hook_server::HookEvent> {
        tokio::sync::broadcast::channel(1).1
    }
    fn attach(
        &self,
        _: &crate::services::claude::herdr_turn::AttachRequest<'_>,
    ) -> Result<bool, String> {
        panic!("no attach")
    }
    fn confirm_bound(
        &self,
        _: &HostedOwner,
        _: &crate::db::dispatched_sessions::hosted_execution::HostedExecution,
        _: &crate::services::session_host::HerdrTarget,
    ) -> Result<(), String> {
        panic!("no bound")
    }
}
impl crate::services::codex::herdr_turn::CodexHerdrPorts for NoLaunch {
    fn launch_host(&self) -> Option<Arc<dyn crate::services::herdr_launch::HerdrLaunchHost>> {
        None
    }
    fn attach(
        &self,
        _: &HostedOwner,
        _: &crate::db::dispatched_sessions::hosted_execution::HostedExecution,
        _: &crate::services::tui_prompt_dedupe::binding_events::SourceId,
        _: &crate::services::session_host::HerdrTarget,
    ) -> Result<bool, String> {
        panic!("no attach")
    }
    fn confirm_bound(
        &self,
        _: &HostedOwner,
        _: &crate::db::dispatched_sessions::hosted_execution::HostedExecution,
        _: &crate::services::session_host::HerdrTarget,
    ) -> Result<(), String> {
        panic!("no bound")
    }
}

pub(in crate::services::discord::turn_bridge) async fn actual_fresh_exit(
    actor: Arc<CancelToken>,
    owner: HostedOwner,
    provider: ProviderKind,
    stop_before: bool,
) {
    tokio::task::spawn_blocking(move || {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .connect_lazy("postgres://unused:unused@127.0.0.1:1/unused")
            .unwrap();
        let endpoint = crate::services::herdr_launch::HerdrLaunchEndpoint {
            execution_node: "test-node".into(),
            config_key: "unused".into(),
            socket_addr: "unused".into(),
            herdr_session: "unused".into(),
        };
        let (sender, _) = std::sync::mpsc::channel();
        if stop_before {
            actor
                .herdr_interrupt_state()
                .unwrap()
                .user_stop
                .store(true, Ordering::Release);
        }
        let result = if provider == ProviderKind::Claude {
            crate::services::claude::herdr_turn::execute(
                crate::services::claude::herdr_turn::HerdrTurn {
                    pool: &pool,
                    channel_id: owner.channel_id.parse().unwrap(),
                    owner,
                    endpoint,
                    row: None,
                    prompt: "cold",
                    working_dir: ".",
                    system_prompt: None,
                    model: None,
                    hook_endpoint: None,
                    cancel: Some(actor.clone()),
                },
                &NoLaunch,
                sender,
            )
        } else {
            crate::services::codex::herdr_turn::execute(
                crate::services::codex::herdr_turn::CodexHerdrTurn {
                    pool: &pool,
                    channel_id: owner.channel_id.parse().unwrap(),
                    owner,
                    endpoint,
                    row: None,
                    prompt: "cold",
                    working_dir: ".",
                    system_prompt: None,
                    model: None,
                    allowed_tools: &[],
                    fast_mode: None,
                    goals: None,
                    compact_token_limit: None,
                    cancel: Some(actor.clone()),
                },
                &NoLaunch,
                sender,
            )
        };
        assert_eq!(result.is_ok(), stop_before);
        if !stop_before {
            actor
                .herdr_interrupt_state()
                .unwrap()
                .user_stop
                .store(true, Ordering::Release);
        }
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "current_thread")]
async fn coldstop_proof_exact_actor_row_notice_and_accounting() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for (provider, stop_before) in [
        (ProviderKind::Claude, false),
        (ProviderKind::Claude, true),
        (ProviderKind::Codex, false),
        (ProviderKind::Codex, true),
    ] {
        let mut shared = discord::make_shared_data_for_tests_with_storage(None);
        let writable = Arc::get_mut(&mut shared).unwrap();
        writable.provider = provider.clone();
        // A nonzero generation, so a close keyed under any other generation is caught.
        writable.restart.current_generation = 17;
        let (actor, owner) = prepared(provider.clone(), 907);
        let mut row = InflightTurnState::new(
            provider.clone(),
            907,
            None,
            1,
            901,
            9_070,
            "cold input".into(),
            None,
            Some(owner.logical_key.clone()),
            None,
            None,
            0,
        );
        row.turn_nonce = actor.turn_nonce().map(str::to_owned);
        let channel = ChannelId::new(row.channel_id);
        assert!(
            discord::mailbox_try_start_turn_kinded(
                &shared,
                channel,
                actor.clone(),
                UserId::new(1),
                MessageId::new(row.user_msg_id),
                crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
            )
            .await
        );
        discord::increment_global_active(&shared, "coldstop_test");
        discord::inflight::save_inflight_state(&row).unwrap();
        actual_fresh_exit(actor.clone(), owner, provider.clone(), stop_before).await;
        let ExitDecision::PolicyClose(proof) = seal_exit(&actor, false, false) else {
            panic!("positive proof required");
        };
        let fin = TurnFinalizer::spawn();
        let key = TurnKey::new(channel, row.user_msg_id, 17)
            .with_episode_nonce(row.turn_nonce.as_deref());
        // A watcher-owned entry, so the generation-17 probe sees exactly this turn's entry.
        fin.register_start(key, provider.clone(), RelayOwnerKind::Watcher, &shared);
        assert!(fin.has_live_watcher_pending(channel, 17).await);
        let anchor = MessageId::new(row.current_msg_id);
        let submitted = test_hooks::submitted();
        let mut stale = proof.clone();
        stale.turn_nonce.push('x');
        assert!(!consume(&shared, &fin, &actor, &row, (&stale, anchor), shown()).await);
        assert!(
            !consume(&shared, &fin, &actor, &row, (&proof, anchor), async {
                Err("notice failed".into())
            })
            .await
        );
        assert!(
            shared
                .mailbox_peek(channel)
                .unwrap()
                .snapshot()
                .await
                .cancel_token
                .is_some()
        );
        assert!(discord::inflight::load_inflight_state_read_only(&provider, 907).is_some());
        for observation in [HerdrSubmission::Submitted, HerdrSubmission::Unknown] {
            actor
                .herdr_interrupt_state()
                .unwrap()
                .submission
                .lock()
                .unwrap()
                .submission = observation;
            assert!(!consume(&shared, &fin, &actor, &row, (&proof, anchor), shown()).await);
            assert!(
                shared
                    .mailbox_peek(channel)
                    .unwrap()
                    .snapshot()
                    .await
                    .cancel_token
                    .is_some()
            );
            assert!(discord::inflight::load_inflight_state_read_only(&provider, 907).is_some());
            assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
        }
        actor
            .herdr_interrupt_state()
            .unwrap()
            .submission
            .lock()
            .unwrap()
            .submission = HerdrSubmission::Unsubmitted;
        assert_eq!(test_hooks::submitted(), submitted, "no refusal submits");
        assert!(consume(&shared, &fin, &actor, &row, (&proof, anchor), shown()).await);
        assert_eq!(
            test_hooks::submitted(),
            submitted + 1,
            "one terminal submission"
        );
        assert!(!fin.has_live_watcher_pending(channel, 17).await);
        assert!(
            shared
                .mailbox_peek(channel)
                .unwrap()
                .snapshot()
                .await
                .cancel_token
                .is_none()
        );
        assert!(discord::inflight::load_inflight_state_read_only(&provider, 907).is_none());
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 0);
        assert!(!consume(&shared, &fin, &actor, &row, (&proof, anchor), shown()).await);
        assert_eq!(
            test_hooks::submitted(),
            submitted + 1,
            "a settled close never resubmits"
        );
        let next = Arc::new(CancelToken::new());
        assert!(
            discord::mailbox_try_start_turn_kinded(
                &shared,
                channel,
                next.clone(),
                UserId::new(1),
                MessageId::new(row.user_msg_id + 1),
                crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
            )
            .await
        );
        assert!(Arc::ptr_eq(
            shared
                .mailbox_peek(channel)
                .unwrap()
                .snapshot()
                .await
                .cancel_token
                .as_ref()
                .unwrap(),
            &next
        ));
    }
}

/// A sealed proof never settles a successor: a replaced mailbox actor or a replaced row
/// nonce refuses the close and leaves the successor's token, row and accounting in place.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_policy_close_spares_successor_actor_and_row() {
    let root = tempfile::tempdir().unwrap();
    let _env = crate::config::set_agentdesk_root_for_test(root.path());
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    for successor_actor in [true, false] {
        let provider = ProviderKind::Codex;
        let mut shared = discord::make_shared_data_for_tests_with_storage(None);
        Arc::get_mut(&mut shared).unwrap().provider = provider.clone();
        let (actor, owner) = prepared(provider.clone(), 908);
        let mut row = InflightTurnState::new(
            provider.clone(),
            908,
            None,
            1,
            911,
            9_080,
            "cold input".into(),
            None,
            Some(owner.logical_key.clone()),
            None,
            None,
            0,
        );
        row.turn_nonce = actor.turn_nonce().map(str::to_owned);
        let channel = ChannelId::new(row.channel_id);
        assert!(
            discord::mailbox_try_start_turn_kinded(
                &shared,
                channel,
                actor.clone(),
                UserId::new(1),
                MessageId::new(row.user_msg_id),
                crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
            )
            .await
        );
        discord::increment_global_active(&shared, "coldstop_test");
        discord::inflight::save_inflight_state(&row).unwrap();
        actual_fresh_exit(actor.clone(), owner, provider.clone(), true).await;
        let ExitDecision::PolicyClose(proof) = seal_exit(&actor, false, false) else {
            panic!("positive proof required");
        };
        let successor = Arc::new(CancelToken::new());
        let mut successor_row = row.clone();
        if successor_actor {
            discord::mailbox_finish_turn(&shared, &provider, channel).await;
            assert!(
                discord::mailbox_try_start_turn_kinded(
                    &shared,
                    channel,
                    successor.clone(),
                    UserId::new(1),
                    MessageId::new(row.user_msg_id + 1),
                    crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
                )
                .await
            );
        } else {
            successor_row.turn_nonce = Some("successor-nonce".into());
            discord::inflight::save_inflight_state(&successor_row).unwrap();
        }
        let fin = TurnFinalizer::spawn();
        let key = TurnKey::new(channel, row.user_msg_id, shared.restart.current_generation)
            .with_episode_nonce(row.turn_nonce.as_deref());
        fin.register_start(key, provider.clone(), RelayOwnerKind::None, &shared);
        let noticed = std::sync::atomic::AtomicBool::new(false);
        let notice = async {
            noticed.store(true, Ordering::Release);
            Ok(ReplaceLongMessageOutcome::EditedOriginal)
        };
        let anchor = MessageId::new(row.current_msg_id);
        assert!(!consume(&shared, &fin, &actor, &row, (&proof, anchor), notice).await);
        // The successor's placeholder is never edited to the stopped notice.
        assert!(!noticed.load(Ordering::Acquire));
        let current = shared
            .mailbox_peek(channel)
            .unwrap()
            .snapshot()
            .await
            .cancel_token
            .expect("the slot stays occupied");
        let expected_actor = if successor_actor { &successor } else { &actor };
        assert!(Arc::ptr_eq(&current, expected_actor));
        let kept = discord::inflight::load_inflight_state_read_only(&provider, 908)
            .expect("the row stays");
        assert_eq!(kept.turn_nonce, successor_row.turn_nonce);
        assert!(
            kept.herdr_before_start_stop.is_none(),
            "no receipt lands on it"
        );
        assert_eq!(shared.restart.global_active.load(Ordering::Relaxed), 1);
    }
}

/// One sealed policy close on a saved row, its mailbox actor and accounting, keyed at generation
/// 17 under its own test root.
struct Close {
    _root: Option<(tempfile::TempDir, crate::config::test_env::TestEnvVarGuard)>,
    _legacy: crate::services::tui_o::cutover::test_override::ChannelsGuard,
    shared: Arc<SharedData>,
    fin: Arc<TurnFinalizer>,
    actor: Arc<CancelToken>,
    row: InflightTurnState,
    proof: BeforeStartProof,
}

impl Close {
    async fn open(channel: u64, shape: impl FnOnce(&mut InflightTurnState)) -> Self {
        let root = tempfile::tempdir().unwrap();
        let env = crate::config::set_agentdesk_root_for_test(root.path());
        let mut close = Self::open_in(17, channel, shape).await;
        close._root = Some((root, env));
        close
    }

    /// Opens under the runtime root the caller already set.
    async fn open_in(
        generation: u64,
        channel: u64,
        shape: impl FnOnce(&mut InflightTurnState),
    ) -> Self {
        let legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
        let provider = ProviderKind::Claude;
        let mut shared = discord::make_shared_data_for_tests_with_storage(None);
        let writable = Arc::get_mut(&mut shared).unwrap();
        writable.provider = provider.clone();
        writable.restart.current_generation = generation;
        let (actor, owner) = prepared(provider.clone(), channel);
        let mut row = InflightTurnState::new(
            provider.clone(),
            channel,
            None,
            1,
            channel + 1,
            channel + 2,
            "cold input".into(),
            None,
            Some(owner.logical_key.clone()),
            None,
            None,
            0,
        );
        row.turn_nonce = actor.turn_nonce().map(str::to_owned);
        shape(&mut row);
        assert!(
            discord::mailbox_try_start_turn_kinded(
                &shared,
                ChannelId::new(channel),
                actor.clone(),
                UserId::new(1),
                MessageId::new(row.user_msg_id),
                crate::services::turn_orchestrator::ActiveTurnKind::UserOrAgent
            )
            .await
        );
        discord::increment_global_active(&shared, "coldstop_test");
        discord::inflight::save_inflight_state(&row).unwrap();
        actual_fresh_exit(actor.clone(), owner, provider.clone(), true).await;
        let ExitDecision::PolicyClose(proof) = seal_exit(&actor, false, false) else {
            panic!("positive proof required");
        };
        let fin = TurnFinalizer::spawn();
        let key = TurnKey::new(ChannelId::new(channel), row.user_msg_id, generation)
            .with_episode_nonce(row.turn_nonce.as_deref());
        fin.register_start(key, provider, RelayOwnerKind::None, &shared);
        Self {
            _root: None,
            _legacy: legacy,
            shared,
            fin,
            actor,
            row,
            proof,
        }
    }

    fn anchor(&self) -> MessageId {
        MessageId::new(self.row.current_msg_id)
    }

    async fn consume(
        &self,
        notice: impl std::future::Future<Output = Result<ReplaceLongMessageOutcome, String>>,
    ) -> bool {
        let close = (&self.proof, self.anchor());
        consume(
            &self.shared,
            &self.fin,
            &self.actor,
            &self.row,
            close,
            notice,
        )
        .await
    }

    fn path(&self) -> std::path::PathBuf {
        discord::inflight::inflight_state_path(
            &discord::inflight::inflight_runtime_root().unwrap(),
            &self.shared.provider,
            self.row.channel_id,
        )
    }

    fn bytes(&self) -> Option<Vec<u8>> {
        std::fs::read(self.path()).ok()
    }

    fn durable(&self) -> Option<InflightTurnState> {
        discord::inflight::load_inflight_state_read_only(&self.shared.provider, self.row.channel_id)
    }

    /// The turn is still held: its actor owns the slot and it is still counted active.
    async fn held(&self) -> bool {
        let slot = self
            .shared
            .mailbox_peek(ChannelId::new(self.row.channel_id));
        let owner = slot.unwrap().snapshot().await.cancel_token;
        owner.is_some_and(|owner| Arc::ptr_eq(&owner, &self.actor))
            && self.shared.restart.global_active.load(Ordering::Relaxed) == 1
    }

    /// What the durable receipt says: `None` without one, else whether its notice is pending.
    fn receipt_pending(&self) -> Option<bool> {
        let receipt = self.durable()?.herdr_before_start_stop?;
        assert!(receipt.supported());
        Some(receipt.notice_pending())
    }
}

/// A notice that records being polled and otherwise shows the notice.
async fn recorded(
    polled: &std::sync::atomic::AtomicBool,
) -> Result<ReplaceLongMessageOutcome, String> {
    polled.store(true, Ordering::Release);
    Ok(ReplaceLongMessageOutcome::EditedOriginal)
}

/// A change applied to a row.
type Shape = fn(&mut InflightTurnState);

/// Nothing is shown or released before the receipt is durable: a failed write keeps the turn held
/// and its row as it was; the notice runs with the receipt on disk and no inflight lock held.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_receipt_precedes_notice_and_release() {
    for rename in [false, true] {
        let armed = if rename { "rename" } else { "write" };
        let close = Close::open(921, |_| {}).await;
        let before = close.bytes();
        let submitted = test_hooks::submitted();
        let polled = std::sync::atomic::AtomicBool::new(false);
        InflightTurnState::arm_before_start_receipt_fault(921, rename);
        assert!(!close.consume(recorded(&polled)).await, "{armed:?}");
        assert!(
            InflightTurnState::before_start_receipt_faults_consumed(921),
            "{armed:?} ran on the receipt write"
        );
        assert!(!polled.load(Ordering::Acquire), "{armed:?}: nothing shown");
        assert!(close.held().await, "{armed:?}");
        assert_eq!(close.bytes(), before, "{armed:?}");
        assert_eq!(
            test_hooks::submitted(),
            submitted,
            "{armed:?}: nothing submitted"
        );
        let path = close.path();
        let durable = || close.durable().and_then(|row| row.herdr_before_start_stop);
        let notice = async {
            assert!(
                durable().is_some_and(|receipt| receipt.notice_pending()),
                "the receipt is durable before the notice"
            );
            assert!(
                discord::inflight::try_lock_inflight_state_path(&path).is_ok(),
                "no inflight lock spans the notice"
            );
            Ok(ReplaceLongMessageOutcome::EditedOriginal)
        };
        assert!(close.consume(notice).await, "{armed:?}");
        assert_eq!(test_hooks::submitted(), submitted + 1, "{armed:?}");
        assert!(close.durable().is_none());
        assert_eq!(
            close.shared.restart.global_active.load(Ordering::Relaxed),
            0
        );
    }
}

/// Only a shown notice is acked; a failed or partial one keeps the receipt pending and the turn
/// held, and a shown one is acked in the receipt before finalize runs.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_notice_ack_only_after_delivery() {
    let close = Close::open(922, |_| {}).await;
    let submitted = test_hooks::submitted();
    let partial = ReplaceLongMessageOutcome::PartialContinuationFailure {
        sent_chunks: 1,
        total_chunks: 2,
        failed_chunk_index: 1,
        sent_continuation_message_ids: Vec::new(),
        cleanup_errors: Vec::new(),
        error: "second chunk".into(),
    };
    for sent in [Err("send failed".to_string()), Ok(partial)] {
        assert!(!close.consume(async { sent }).await);
        assert_eq!(close.receipt_pending(), Some(true));
        assert!(close.held().await);
        assert_eq!(test_hooks::submitted(), submitted);
    }
    let finalized_on = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let seen = finalized_on.clone();
    test_hooks::observe(Some(Box::new(move |step, _| {
        if step == "before_finalize" {
            let durable =
                discord::inflight::load_inflight_state_read_only(&ProviderKind::Claude, 922);
            let receipt = durable.and_then(|row| row.herdr_before_start_stop).unwrap();
            seen.borrow_mut()
                .push(receipt.supported() && !receipt.notice_pending());
        }
    })));
    let fallback = ReplaceLongMessageOutcome::SentFallbackAfterEditFailure {
        edit_error: "gone".into(),
        replacement_anchor: None,
    };
    let settled = close.consume(async { Ok(fallback) }).await;
    test_hooks::observe(None);
    assert!(settled);
    assert_eq!(*finalized_on.borrow(), [true], "acked before finalize");
    assert_eq!(test_hooks::submitted(), submitted + 1);
    assert!(close.durable().is_none());
}

/// A successor written between the seal and the receipt's lock gets no receipt and keeps its
/// bytes, slot and accounting, whichever birth axis tells it apart.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_receipt_admission_spares_successor_row() {
    let successors: [(&str, Shape); 3] = [
        ("nonce", |row| {
            row.turn_nonce = Some("successor-nonce".into())
        }),
        ("start and generation", |row| {
            row.started_at.push('9');
            row.born_generation += 1;
        }),
        ("same message id", |row| {
            row.turn_nonce = Some("successor-nonce".into());
            row.started_at.push('9');
        }),
    ];
    for (case, successor) in successors {
        let close = Close::open(923, |_| {}).await;
        let mut next = close.row.clone();
        successor(&mut next);
        let written = std::rc::Rc::new(std::cell::RefCell::new(None));
        let slot = written.clone();
        test_hooks::observe(Some(Box::new(move |step, _| {
            if step == "before_receipt" {
                discord::inflight::save_inflight_state(&next).unwrap();
                let path = discord::inflight::inflight_state_path(
                    &discord::inflight::inflight_runtime_root().unwrap(),
                    &ProviderKind::Claude,
                    923,
                );
                *slot.borrow_mut() = std::fs::read(path).ok();
            }
        })));
        let polled = std::sync::atomic::AtomicBool::new(false);
        let settled = close.consume(recorded(&polled)).await;
        test_hooks::observe(None);
        assert!(!settled, "{case}");
        assert!(!polled.load(Ordering::Acquire), "{case}: nothing shown");
        assert_eq!(close.bytes(), *written.borrow(), "{case}: successor bytes");
        assert!(close.held().await, "{case}");
    }
}

/// The bridge shows the stop only where it owns the body: an O-owned destination or a message
/// anchor that moved keeps the receipt pending, sends nothing and holds the turn.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_bridge_shows_the_stop_only_on_its_own_body() {
    let close = Close::open(924, |row| {
        row.runtime_kind = Some(crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui)
    })
    .await;
    let polled = std::sync::atomic::AtomicBool::new(false);
    {
        let _o = crate::services::tui_o::cutover::test_override::force_channels(&[(
            924,
            crate::services::agent_protocol::RuntimeHandoffKind::ClaudeTui,
        )]);
        assert!(!close.consume(recorded(&polled)).await);
    }
    assert!(!polled.load(Ordering::Acquire), "O owns the body");
    assert_eq!(close.receipt_pending(), Some(true));
    assert!(close.held().await);
    let mut moved = close.durable().unwrap();
    moved.current_msg_id += 1;
    discord::inflight::save_inflight_state(&moved).unwrap();
    assert!(!close.consume(recorded(&polled)).await);
    assert!(!polled.load(Ordering::Acquire), "the anchor moved");
    assert_eq!(close.receipt_pending(), Some(true));
    assert!(close.held().await);
}

const CRASH_FILTER: &str = "coldstop_restart_finishes_a_close_that_crashed_midway";
const CRASH_PHASE: &str = "ADK_COLDSTOP_B_PHASE";
const CRASH_ROOT: &str = "ADK_COLDSTOP_B_ROOT";
const CRASH_CHANNEL: u64 = 1_479_671_301_387_210_000;

/// Runs this test as `phase` in a new process and returns the pid it printed beside `marker`.
fn run_crash_phase(phase: &str, marker: &str, env: &[(&str, &std::ffi::OsStr)]) -> u32 {
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command.args([CRASH_FILTER, "--test-threads=1", "--nocapture"]);
    command.env(CRASH_PHASE, phase);
    for (key, value) in env {
        command.env(key, value);
    }
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{phase}: {stdout}\n{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let line = stdout
        .lines()
        .find(|line| line.contains(marker))
        .unwrap_or_else(|| panic!("{phase} printed no {marker}: {stdout}"));
    let pid = line.split("pid=").nth(1).unwrap().trim().parse().unwrap();
    assert_ne!(pid, std::process::id());
    pid
}

/// A close dying after its receipt, notice ack or just before finalize is finished by the next
/// restart, led by the receipt alone (no Herdr marker): the row goes, the stop shows once in total.
#[tokio::test(flavor = "current_thread")]
async fn coldstop_restart_finishes_a_close_that_crashed_midway() {
    if let Ok(phase) = std::env::var(CRASH_PHASE) {
        return crash_phase(&phase).await;
    }
    for step in ["after_receipt", "after_notice_ack", "before_finalize"] {
        let root = tempfile::tempdir().unwrap();
        let close = [
            (CRASH_ROOT, root.path().as_os_str()),
            (test_hooks::STOP_AT, std::ffi::OsStr::new(step)),
        ];
        let restore = [(CRASH_ROOT, root.path().as_os_str())];
        let closed = run_crash_phase("CLOSE", "COLDSTOP_B_STOP", &close);
        let restored = run_crash_phase("RESTORE", "COLDSTOP_B_PHASE RESTORE", &restore);
        assert_ne!(closed, restored);
        let shown = std::fs::read_to_string(root.path().join("notices.log")).unwrap_or_default();
        let expected = if step == "after_receipt" {
            ["restart"]
        } else {
            ["bridge"]
        };
        assert_eq!(shown.lines().collect::<Vec<_>>(), expected, "{step}");
    }
}

async fn crash_phase(phase: &str) {
    let root = std::path::PathBuf::from(std::env::var_os(CRASH_ROOT).unwrap());
    let log = root.join("notices.log");
    let note = |by: &str| {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .unwrap();
        writeln!(file, "{by}").unwrap();
    };
    let _env = crate::config::set_agentdesk_root_for_test(&root);
    // Boot allocates the process generation that `SharedData` and every row carry.
    let generation = crate::services::discord::runtime_store::allocate_process_generation();
    if phase == "CLOSE" {
        let close = Close::open_in(generation, CRASH_CHANNEL, |_| {}).await;
        assert_eq!(
            close.shared.restart.current_generation,
            crate::services::discord::runtime_store::process_generation()
        );
        close
            .consume(async {
                note("bridge");
                Ok(ReplaceLongMessageOutcome::EditedOriginal)
            })
            .await;
        unreachable!("the step hook ends this process");
    }
    let _legacy = crate::services::tui_o::cutover::test_override::force_channels(&[]);
    assert_eq!(
        generation,
        crate::services::discord::runtime_store::process_generation()
    );
    let mut shared = discord::make_shared_data_for_tests_with_storage(None);
    let writable = Arc::get_mut(&mut shared).unwrap();
    writable.provider = ProviderKind::Claude;
    writable.restart.current_generation = generation;
    let row =
        discord::inflight::load_inflight_state_read_only(&ProviderKind::Claude, CRASH_CHANNEL)
            .expect("the crashed close left its row");
    let receipt = row.herdr_before_start_stop.expect("with its receipt");
    assert!(receipt.supported() && receipt.admitted_generation() < generation);
    let recorder =
        crate::services::discord::recovery_engine::o_cut_recorder::start(CRASH_CHANNEL).await;
    crate::services::discord::recovery_engine::restore_inflight_turns(
        &recorder.http,
        &shared,
        &ProviderKind::Claude,
    )
    .await;
    let route = format!("/channels/{CRASH_CHANNEL}/messages");
    for call in recorder.calls() {
        assert!(call.route.contains(&route), "{:?}", call.route);
        assert_eq!(call.content.as_deref(), Some(ADMITTED_ABORT_NOTICE));
        note("restart");
    }
    assert!(
        discord::inflight::load_inflight_state_read_only(&ProviderKind::Claude, CRASH_CHANNEL)
            .is_none(),
        "restart finished the close"
    );
    println!("COLDSTOP_B_PHASE RESTORE pid={}", std::process::id());
}
