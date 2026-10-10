//! Test-only in-process policy path; durable receipt and production wiring are not present.
use crate::services::discord::inflight::{InflightTurnIdentity, InflightTurnState, RelayOwnerKind};
use crate::services::discord::turn_finalizer::{
    FinalizeContext, FinalizeOutcome, SyntheticClaimSnapshot, TerminalEvent, TurnFinalizer, TurnKey,
};
use crate::services::discord::{self, SharedData};
use crate::services::provider::CancelToken;
#[cfg(unix)]
use crate::services::provider::ProviderKind;
use crate::services::provider::herdr_before_start::{BeforeStartProof, ExitDecision, seal_exit};
use poise::serenity_prelude::{ChannelId, MessageId, UserId};
use std::sync::Arc;

async fn consume(
    shared: &Arc<SharedData>,
    fin: &TurnFinalizer,
    actor: &Arc<CancelToken>,
    row: &InflightTurnState,
    proof: &BeforeStartProof,
    notice: impl std::future::Future<Output = Result<(), String>>,
) -> bool {
    let Some(state) = actor.herdr_interrupt_state() else {
        return false;
    };
    let expected = InflightTurnIdentity::from_state(row);
    if proof.owner != state.owner
        || proof.owner.discord_token_hash != shared.token_hash
        || proof.owner.provider != row.provider
        || proof.owner.channel_id != row.channel_id.to_string()
        || row.tmux_session_name.as_deref() != Some(&proof.owner.logical_key)
        || row.turn_nonce.as_deref() != Some(&proof.turn_nonce)
        || actor.turn_nonce() != Some(&proof.turn_nonce)
        || actor.claude_interrupt_generation() != proof.generation
        || state.closed_probe() != Some(true)
        || !discord::inflight::load_inflight_state_read_only(&shared.provider, row.channel_id)
            .is_some_and(|current| {
                expected.matches_state(&current) && current.turn_nonce == row.turn_nonce
            })
    {
        return false;
    }
    let channel = ChannelId::new(row.channel_id);
    let Some(mailbox) = shared.mailbox_peek(channel) else {
        return false;
    };
    if !mailbox
        .snapshot()
        .await
        .cancel_token
        .as_ref()
        .is_some_and(|current| Arc::ptr_eq(current, actor))
    {
        return false;
    }
    if notice.await.is_err() {
        return false;
    }
    let key = TurnKey::new(channel, row.effective_finalizer_turn_id(), 0)
        .with_episode_nonce(row.turn_nonce.as_deref());
    let mut snapshot = SyntheticClaimSnapshot::from_row(row);
    snapshot.recovery_actor = Some(Arc::downgrade(actor));
    let context = FinalizeContext {
        clear_inflight: true,
        ..FinalizeContext::bridge()
    };
    matches!(
        fin.submit_terminal_with_claim_snapshot(
            key,
            shared.provider.clone(),
            TerminalEvent::Cancel,
            context,
            Some(snapshot),
            shared.clone()
        )
        .await,
        FinalizeOutcome::Finalized { .. }
    )
}

#[cfg(unix)]
mod tests {
    use super::*;
    use crate::db::dispatched_sessions::hosted_execution::HostedOwner;
    use crate::services::provider::cancel_token_claude_interrupt::{
        HERDR_SETTLEMENT_OVERRIDE, HerdrSubmission,
    };
    use crate::services::provider::herdr_before_start::{
        InputPhase, finish_execution, prelaunch_closed,
    };
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
                // The unchanged production hold still retains every cold intent before A2 wiring.
                assert!(
                    discord::turn_bridge::stream_loop::exit_reconcile::herdr_stop_unconfirmed(
                        &actor, false, false
                    )
                );
            }
            for submission in [HerdrSubmission::Submitted, HerdrSubmission::Unknown] {
                let (actor, _) = prepared(provider.clone(), 902);
                let state = actor.herdr_interrupt_state().unwrap();
                state.submission.lock().unwrap().submission = submission;
                state.user_stop.store(true, Ordering::Release);
                assert_eq!(seal_exit(&actor, false, false), ExitDecision::Hold);
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
        assert!(
            !discord::turn_bridge::stream_loop::exit_reconcile::herdr_stop_unconfirmed(
                &actor, false, false
            )
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

    struct NoLaunch;
    impl crate::services::claude::herdr_turn::HerdrTurnPorts for NoLaunch {
        fn launch_host(&self) -> Option<Arc<dyn crate::services::herdr_launch::HerdrLaunchHost>> {
            None
        }
        fn hook_events(
            &self,
        ) -> tokio::sync::broadcast::Receiver<crate::services::claude_tui::hook_server::HookEvent>
        {
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

    async fn actual_fresh_exit(
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
        for (provider, stop_before) in [
            (ProviderKind::Claude, false),
            (ProviderKind::Claude, true),
            (ProviderKind::Codex, false),
            (ProviderKind::Codex, true),
        ] {
            let shared = discord::make_shared_data_for_tests_with_storage(None);
            let mut shared = shared;
            Arc::get_mut(&mut shared).unwrap().provider = provider.clone();
            let (actor, owner) = prepared(provider.clone(), 907);
            let mut row = InflightTurnState::new(
                provider.clone(),
                907,
                None,
                1,
                901,
                0,
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
            let key = TurnKey::new(channel, row.user_msg_id, 0)
                .with_episode_nonce(row.turn_nonce.as_deref());
            fin.register_start(key, provider.clone(), RelayOwnerKind::None, &shared);
            let mut stale = proof.clone();
            stale.turn_nonce.push('x');
            assert!(!consume(&shared, &fin, &actor, &row, &stale, async { Ok(()) }).await);
            assert!(
                !consume(&shared, &fin, &actor, &row, &proof, async {
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
                assert!(!consume(&shared, &fin, &actor, &row, &proof, async { Ok(()) }).await);
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
            assert!(consume(&shared, &fin, &actor, &row, &proof, async { Ok(()) }).await);
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
            assert!(!consume(&shared, &fin, &actor, &row, &proof, async { Ok(()) }).await);
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
}
