//! A Herdr start whose producer unwound it before its first key, and the attempt's own row.
use super::*;
use crate::services::discord::inflight::{self, InflightTurnState, row_removed};
use crate::services::discord::queue_io::input_owner::drive::claim_turn;
use crate::services::discord::router::message_handler::foreign_row;
use crate::services::provider::cancel_token_claude_interrupt::HerdrSubmission;

const HELD: &str = "NotSent(\"held: PreparationHold { phase: BeforePayload }\")";

fn row(channel: ChannelId, id: u64, token: &CancelToken) -> InflightTurnState {
    let mut row = InflightTurnState::new(
        ProviderKind::Claude,
        channel.get(),
        None,
        7,
        id,
        0,
        "our input".into(),
        None,
        None,
        None,
        None,
        0,
    );
    row.turn_nonce = token.turn_nonce().map(str::to_string);
    row
}

/// The producer's own pre-key unwind cancels the attempt's token as cleanup; only a stop the
/// person took keeps the row. Without the rewind the same front's next start is refused.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_producer_unwound_pre_key_start_still_rewinds_its_own_row() {
    for (channel, stop) in [(8_441_501, false), (8_441_502, true)] {
        let guard = crate::config::TestRuntimeRootGuard::new();
        let _binding_root = native_prepare::BindingRoot::enter(Some(guard.path()));
        let root = guard.path().join("runtime");
        std::fs::create_dir_all(&root).unwrap();
        let id = 8_000_000_000_000_000_000 + channel;
        let channel = ChannelId::new(channel);
        let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(None);
        let restart = &mut std::sync::Arc::get_mut(&mut shared).unwrap().restart;
        // The row is born in this process's generation, which the native producer's key names.
        let path = crate::services::discord::runtime_store::generation_path().unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "3").unwrap();
        let generation = crate::services::discord::runtime_store::process_generation();
        restart.current_generation = generation;
        restart
            .global_active
            .store(1, std::sync::atomic::Ordering::SeqCst);
        let token = std::sync::Arc::new(CancelToken::new());
        let owner = crate::db::dispatched_sessions::hosted_execution::HostedOwner {
            provider: "claude".into(),
            discord_token_hash: shared.token_hash.clone(),
            channel_id: channel.to_string(),
            logical_key: format!("native-prekey-{channel}"),
            owner_node: "test-node".into(),
            runtime_root: "test-root".into(),
        };
        let state = token.prepare_herdr_interrupt(ProviderKind::Claude, &owner);
        let mailbox = shared.mailbox(channel);
        let user = poise::serenity_prelude::UserId::new(7);
        mailbox
            .restore_active_turn(token.clone(), user, id.into())
            .await;
        let key = TurnKey::new(channel, id, generation).with_episode_nonce(token.turn_nonce());
        let relay = RelayOwnerKind::Watcher;
        (shared.turn_finalizer).register_start(key, ProviderKind::Claude, relay, &shared);
        let persistence = crate::services::turn_orchestrator::QueuePersistenceContext::new(
            &ProviderKind::Claude,
            &shared.token_hash,
            None,
        );
        let (queue, namespace) =
            crate::services::turn_orchestrator::v2_queue(&persistence, channel).unwrap();
        mailbox.v2_adopt(queue, namespace).await.unwrap();
        let item=serde_json::from_value(json!({"author_id":7,"message_id":id,"channel_id":channel.get(),"source_message_ids":[id],"queued_generation":1,"text":"our input"})).unwrap();
        let _ = admit(&mailbox, &item, AdmissionTicket::new()).await;
        let attempt = cold_attempt(&root, channel.get()).0;
        // A Herdr turn's row names the execution its first input goes to.
        let mut first = row(channel, id, &token);
        first.hosted_execution_nonce = attempt["target"]["execution_nonce"]
            .as_str()
            .map(Into::into);
        inflight::save_inflight_state_create_new(&first).unwrap();
        let producer = Producer::at(shared.clone(), key, Some(token.clone()));
        let intent = producer.intent.clone();
        let prepared = PreparedPayload {
            frame: "our input".into(),
            sources: vec![id.to_string()],
            attempt,
        };
        // The first input's own record of a composer it found Running: no key reached the pane.
        let leaf: effect::Leaf = Box::new(move |_| {
            let mut input = state.submission.lock().unwrap();
            assert!(state.prepare_input(&mut input));
            state.finish_input(&mut input, HerdrSubmission::Unsubmitted, true);
            scope::vetoed("composer running");
            Ok(Completion::Held("composer running".into()))
        });
        // The owner's claim goes to the pump as the launch hook hands it on.
        let granted = mailbox.v2_request(|slot| {
            let owner = slot.as_mut().ok_or("owner")?;
            let front = owner.front().ok_or("front")?;
            owner.grant_front(&front).map(|_| front)
        });
        let front = granted.await.unwrap();
        let claim = claim_turn(&mailbox, id, &front).await.unwrap().unwrap();
        let failure = super::super::super::super::start_failure::Failure::claimed(&mailbox, &claim);
        let clock = std::sync::Arc::new(|| 10_000);
        let wrapped = producer.wrap(leaf);
        let claim = Some(claim);
        let submitted =
            pump::submit_native_front(&mailbox, claim, prepared, intent, clock, wrapped);
        let submitted = await_within(submitted, WAIT_CAP).await.unwrap();
        assert_eq!(submitted, Completion::NotSent("composer running".into()));
        assert!(
            token.is_completion_cleanup()
                && token.cancelled.load(std::sync::atomic::Ordering::Acquire)
        );
        if stop {
            token.publish_cancel("explicit_stop");
        }
        let watch = row_removed::watch(&ProviderKind::Claude, channel.get());
        let host = TurnHost::Tmux;
        let turn = StreamingTurn {
            pool: None,
            registration_origin: None,
            agy_foreground: None,
            shared: Some(&shared),
            provider: &ProviderKind::Claude,
            prompt: "our input",
            session_id: None,
            working_dir: "/tmp",
            system_prompt: None,
            allowed_tools: &[],
            cancel: token.clone(),
            codex_observation: None,
            tmux_session_name: None,
            teardown: None,
            host: &host,
            channel_id: channel.get(),
            model: None,
            native_fast_mode: None,
            effort: None,
            codex_goals: None,
            compact_percent: None,
            compact_lower_bound_tokens: 0,
            compact_token_limit: None,
            cache_ttl_minutes: None,
            dispatch_type: None,
            force_fresh: false,
            submission: None,
        };
        assert_eq!(
            failure.finish(&turn, watch, Err(HELD.into())),
            Err(HELD.into())
        );

        let retry = std::sync::Arc::new(CancelToken::new());
        let mut next = row(channel, id, &retry);
        next.set_followup_requeue_context(None, false, false, Vec::new(), true);
        let create =
            |state: &mut InflightTurnState| inflight::save_inflight_state_create_new(state);
        let verdict = foreign_row::admit(&shared, &ProviderKind::Claude, &mut next, &retry, create);
        let admitted = verdict.await == Ok(true);
        let durable = inflight::load_inflight_state_read_only(&ProviderKind::Claude, channel.get());
        let nonce = durable.and_then(|row| row.turn_nonce);
        let kept = if stop { &token } else { &retry };
        let expected = (!stop, kept.turn_nonce().map(str::to_string));
        assert_eq!((admitted, nonce), expected, "stop={stop}");
    }
}
