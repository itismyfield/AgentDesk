//! A scheduled start under the channel's own key on a bound Herdr execution: it waits for that
//! execution's idle, then runs once as its own turn.
use super::*;
use crate::test_support::wait::{WAIT_CAP, await_within};

#[test]
fn internal_herdr_channel_key_scheduled_start_waits_for_idle_and_runs_once_pg() {
    use crate::services::claude_tui::busy_inject::Screen;
    use crate::services::discord::queue_io::input_owner::held_watch;
    use discord::router::message_handler::provider_dispatch::OwnerTurn;
    let fx = Fixture::new("http-internal-channel", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch_pinless(&fx, &launcher);
    let before = fx.rig.sends().len();
    let registry_guard = fx.rig.registry_on_this_thread();
    let admission_guard = open_admission();
    let intake = Intake::with_expected_pin(&fx, &started, ExpectedPin::Registered);
    // A native turn AgentDesk did not start holds the channel's execution.
    native_busy(&started);
    let heads = answer_input(&fx, &intake, &started);
    let fallback = FallbackCount::new();
    intake.runtime.block_on(async {
        let _router = app(&fx, &intake).await;
        let channel = ChannelId::new(CHANNEL);
        let turn = discord::router::reserve_headless_turn().turn_id(channel);
        // What the scheduled launcher keeps for a delivery with no snapshot: no label of its own.
        let metadata = json!({"agent_id":"busy-entry","scheduled_message_id":"smsg-8125"});
        let context = json!({"origin":"scheduled","provider":"claude","turn_id":turn,
            "metadata":metadata,"synthetic_label":null,"source":"scheduled_message",
            "channel_name_hint":fx.logical().strip_prefix("AgentDesk-claude-"),"is_dm":false});
        let registry = Arc::new(HealthRegistry::new());
        registry
            .register("claude".into(), intake.shared.clone())
            .await;
        let http = intake.shared.serenity_http_or_token_fallback().unwrap();
        registry.register_http("claude".into(), http).await;
        // With no owner holding it the same start never reaches the Herdr executor.
        use crate::services::discord::health as entry;
        let unowned = entry::start_reserved_headless_agent_turn_with_owner_channel(
            &registry,
            channel,
            channel,
            ProviderKind::Claude,
            INPUT.to_string(),
            Some("scheduled_message".to_string()),
            Some(metadata.clone()),
            None,
            None,
            entry::reserve_headless_agent_turn(channel),
        );
        let unowned = unowned
            .await
            .map(drop)
            .map_err(|error| format!("{error:?}"));
        assert!(
            unowned
                .as_ref()
                .is_err_and(|error| error.contains("executor_not_wired")),
            "{unowned:?}"
        );
        let drives = OwnerTurn::drive_results(CHANNEL).len();
        let execution = ChannelMailboxHandle::owner_input_execution("claude", CHANNEL, false);
        let owned = intake
            .mailbox
            .owned_start(execution, (&turn, CHANNEL, INPUT));
        let owned = discord::OWNER_ADOPTS_FOR_TEST.scope(true, owned).await;
        let (original, namespace) = owned.unwrap().unwrap();
        let input =
            ChannelMailboxHandle::new_owner_input(&original, &namespace, "claude", context.clone());
        assert!(input.confirm_admission(&intake.mailbox).await.unwrap());
        ChannelMailboxHandle::owner_input_wake(&registry, "claude", CHANNEL).await;
        let driven = async {
            while OwnerTurn::drive_results(CHANNEL).len() == drives {
                tokio::task::yield_now().await;
            }
        };
        await_within(driven, WAIT_CAP).await;
        // The start read the Herdr execution, kept its original unclaimed and wrote nothing.
        let screen = OwnerTurn::start_wait_screen_for_test(&intake.shared, channel, &context).await;
        assert!(
            matches!(screen, Some(Screen::Blocked | Screen::Unknown)),
            "{screen:?}"
        );
        let (_, looked) =
            held_watch::watch_observed(source_retry_drive(&intake), original.message_id);
        let look = await_within(looked, WAIT_CAP).await.unwrap();
        assert_eq!(look.verdict, held_watch::Verdict::Keep);
        assert_eq!(input.start_rejection(&intake.mailbox).await.unwrap(), None);
        assert_eq!(fx.rig.sends().len(), before);
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(false));
        append(
            &started.path,
            &[json!({"type":"system","subtype":"turn_duration",
            "sessionId":started.session,"timestamp":chrono::Utc::now().to_rfc3339()})],
        );
        // The native terminal alone lets it run: one input, one answer from O, no other driver.
        completed(&intake, 1).await;
        assert_eq!(input.start_rejection(&intake.mailbox).await.unwrap(), None);
        assert_eq!(fx.rig.sends().len(), before + 2);
        assert!(
            fx.rig.sends()[before]["text"]
                .as_str()
                .unwrap()
                .contains(INPUT)
        );
        assert_eq!(heads.lock().unwrap().len(), 2);
        fallback.assert_none();
        assert_eq!(launcher.creates.load(Ordering::SeqCst), 1);
        // The delivery's poller closes on this turn's transcript.
        let evidence = async {
            loop {
                let rows: i64 = sqlx::query_scalar(
                    "SELECT COUNT(*) FROM session_transcripts
                     WHERE turn_id = $1 AND BTRIM(assistant_message) <> ''",
                )
                .bind(&turn)
                .fetch_one(&fx.pool)
                .await
                .unwrap();
                if rows > 0 {
                    break rows;
                }
                tokio::task::yield_now().await;
            }
        };
        assert_eq!(await_within(evidence, WAIT_CAP).await, 1);
    });
    drop(intake);
    drop(admission_guard);
    drop(registry_guard);
}

#[test]
fn e0_herdr_channel_key_routine_withdraws_while_waiting_for_idle_pg() {
    use crate::services::claude_tui::busy_inject::Screen;
    use crate::services::discord::queue_io::input_owner::held_watch;
    use crate::services::routines::{NewRoutine, RoutineAgentExecutor, RoutineStore};
    use discord::router::message_handler::provider_dispatch::OwnerTurn;

    let fx = Fixture::new("routine-settlement-herdr-idle", None);
    let launcher = Arc::new(Launcher::default());
    let (_, started) = reconnect::launch_pinless(&fx, &launcher);
    let before = fx.rig.sends().len();
    let registry_guard = fx.rig.registry_on_this_thread();
    let admission_guard = open_admission();
    let intake = Intake::with_expected_pin(&fx, &started, ExpectedPin::Registered);
    native_busy(&started);
    intake.runtime.block_on(async {
        let _router = app(&fx, &intake).await;
        let channel = ChannelId::new(CHANNEL);
        let registry = Arc::new(HealthRegistry::new());
        registry
            .register("claude".into(), intake.shared.clone())
            .await;
        let http = intake.shared.serenity_http_or_token_fallback().unwrap();
        registry.register_http("claude".into(), http).await;

        let pool = Arc::new(fx.pool.clone());
        let store =
            RoutineStore::new_with_timezone_and_checkpoint_limit(pool.clone(), "UTC", 262144);
        let now = chrono::Utc::now();
        store
            .attach_routine(NewRoutine {
                agent_id: Some("busy-entry".into()),
                fallback_agent_id: None,
                max_retries: Some(0),
                script_ref: "settlement.js".into(),
                name: "Herdr idle settlement".into(),
                status: None,
                execution_strategy: "persistent".into(),
                schedule: None,
                next_due_at: Some(now - chrono::Duration::minutes(1)),
                checkpoint: None,
                discord_thread_id: None,
                timeout_secs: Some(60),
            })
            .await
            .unwrap();
        let run = store.claim_due_runs(1).await.unwrap().pop().unwrap();
        let turn = discord::router::reserve_headless_turn().turn_id(channel);
        let context = json!({
            "status":"input_pending", "origin":"routine","provider":"claude","turn_id":turn,
            "run_id":run.run_id,"routine_id":run.routine_id,"agent_id":"busy-entry",
            "execution_strategy":"persistent","execution_mode":"tmux","source":"routine",
            "metadata":{
                "agent_id":"busy-entry","routine_id":run.routine_id,"routine_run_id":run.run_id,
                "execution_strategy":"persistent","execution_mode":"tmux","turn_id":turn
            },
            "channel_id":CHANNEL.to_string(),"synthetic_label":null,"is_dm":false,
            "channel_name_hint":fx.logical().strip_prefix("AgentDesk-claude-"),
            (ChannelMailboxHandle::ROUTINE_DELIVERY_DEADLINE):now.timestamp_millis()
        });
        let execution = ChannelMailboxHandle::owner_input_execution("claude", CHANNEL, false);
        let owned = intake
            .mailbox
            .owned_start(execution, (&turn, CHANNEL, INPUT));
        let (original, namespace) = discord::OWNER_ADOPTS_FOR_TEST
            .scope(true, owned)
            .await
            .unwrap()
            .unwrap();
        let input =
            ChannelMailboxHandle::new_owner_input(&original, &namespace, "claude", context.clone());
        let mut intent = context.clone();
        intent["owner_input"] = json!(input);
        assert!(
            store
                .record_agent_input_intent(&run.run_id, None, &turn, &intent)
                .await
                .unwrap()
        );
        assert!(input.confirm_admission(&intake.mailbox).await.unwrap());
        let drives = OwnerTurn::drive_results(CHANNEL).len();
        ChannelMailboxHandle::owner_input_wake(&registry, "claude", CHANNEL).await;
        await_within(
            async {
                while OwnerTurn::drive_results(CHANNEL).len() == drives {
                    tokio::task::yield_now().await;
                }
            },
            WAIT_CAP,
        )
        .await;
        assert!(matches!(
            OwnerTurn::start_wait_screen_for_test(&intake.shared, channel, &context).await,
            Some(Screen::Blocked | Screen::Unknown)
        ));
        let (_, looked) =
            held_watch::watch_observed(source_retry_drive(&intake), original.message_id);
        assert_eq!(
            await_within(looked, WAIT_CAP).await.unwrap().verdict,
            held_watch::Verdict::Keep
        );

        let executor = RoutineAgentExecutor::new(pool, Some(registry), 60);
        let outcomes = executor.poll_agent_runs(&store, 10, false).await.unwrap();
        assert_eq!(outcomes.len(), 1);
        assert_eq!(outcomes[0].status, "failed");
        assert_eq!(
            outcomes[0].result_json.as_ref().unwrap()["status"],
            "channel_input_unavailable"
        );
        let owner = intake
            .mailbox
            .v2_request(|slot| slot.as_ref().ok_or("owner missing")?.json())
            .await
            .unwrap();
        assert_eq!(owner["items"], json!([]));
        assert!(owner["head_injection"].is_null());
        assert_eq!(owner["retained_inputs"], json!([]));
        assert_eq!(fx.rig.sends().len(), before);
    });
    drop(intake);
    drop(admission_guard);
    drop(registry_guard);
}
