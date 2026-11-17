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
    let (_, started) = reconnect::launch(&fx, &launcher);
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
