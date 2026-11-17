//! Actual idle starts, including a queue restored after startup retired its missing pane.
use super::*;
use crate::services::discord::router::message_handler::provider_dispatch as dispatch;
use crate::services::discord::shared_state::test_rest;

fn recording_runtime(intake: &mut Intake, fx: &Fixture, launch: bool) -> tokio::runtime::Runtime {
    let install: Box<dyn Fn() + Send + Sync> = if launch {
        Box::new(fx.rig.thread_installer_with_launch(NODE, KEY, SESSION))
    } else {
        Box::new(fx.rig.thread_installer(NODE, KEY, fx.rig.socket(), SESSION))
    };
    let config = crate::config_live_reload::current().unwrap();
    let boot = cutover::boot_snapshot().expect("the managed writer's adopted boot snapshot");
    let root = crate::services::tui_prompt_dedupe::binding_events::test_root();
    let http = intake._api.http.clone();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .on_thread_start(move || {
            install();
            crate::services::tui_prompt_dedupe::binding_events::set_test_root(root.as_deref());
            WORKER_GUARDS.with(|guards| {
                guards.borrow_mut().extend([
                    Box::new(test_override::force_boot(boot.clone())) as Box<dyn std::any::Any>,
                    Box::new(open_admission()),
                    Box::new(crate::services::herdr_launch::force_writer_accepts(Some(
                        true,
                    ))),
                    Box::new(crate::config::session_hosts::force_config_for_test(&config)),
                    Box::new(test_rest::install(http.clone())),
                ])
            });
        })
        .on_thread_stop(|| WORKER_GUARDS.with(|guards| guards.borrow_mut().clear()))
        .build()
        .unwrap();
    std::mem::replace(&mut intake.runtime, runtime)
}

fn idle_case(missing: bool, restored_after_reconnect: bool) {
    let fx = Fixture::new("missing-idle", None);
    let launcher = Arc::new(Launcher::default());
    let (nonce, started) = reconnect::launch(&fx, &launcher);
    let _registry = fx.rig.registry_on_this_thread();
    let _admission = open_admission();
    let _gate = discord::router::intake_gate::busy_inject::test_support::open_gate(CHANNEL);
    let original_runtime;
    let mut intake = Intake::new(&fx, &started);
    let _writer = crate::services::herdr_launch::force_writer_accepts(Some(true));
    original_runtime = recording_runtime(&mut intake, &fx, false);
    fx.rig.answer("pane.process_info", json!({"type":"ok"}));
    if missing {
        fx.rig.show_panes(&[]);
    } else {
        fx.rig.answer("session.snapshot", json!({"type":"ok"}));
    }
    if restored_after_reconnect {
        crate::services::tui_prompt_dedupe::release_herdr_execution(fx.logical(), &nonce);
        let _entered = intake.runtime.enter();
        crate::services::discord::recovery_engine::herdr_reader::reconnect_restarted_herdr_with_shared(&intake.shared, &ProviderKind::Claude);
        assert_eq!(fx.row(), Some(HostedState::Retired));
    }
    intake.runtime.block_on(async {
        let key = discord::adk_session::turn_session_key(
            &intake.shared,
            ChannelId::new(CHANNEL),
            &ProviderKind::Claude,
        )
        .await
        .unwrap();
        let host = crate::services::turn_host::for_turn(
            Some(&fx.pool),
            &ProviderKind::Claude,
            CHANNEL,
            Some(&key),
        )
        .await;
        let TurnHost::Herdr(plan) = &host else {
            panic!("the actual idle start is admitted on Herdr");
        };
        assert!(
            plan.row.as_ref().is_some_and(|row| matches!(&row.record,
            HostedRecord::Known(record) if record.state == if restored_after_reconnect {
                HostedState::Retired
            } else { HostedState::Bound })),
            "the idle start reads the seeded row: {:?}",
            plan.row.as_ref().map(|row| &row.record)
        );
        if !restored_after_reconnect {
            assert_eq!(
                claude_handoff::observe_busy(
                    CHANNEL,
                    fx.logical(),
                    &host,
                    &intake.shared.token_hash
                )
                .state(),
                crate::services::claude_tui::host_input::busy::BusyState::Held
            );
        }
        assert_eq!(intake.mailbox.has_active_turn().await, Ok(false));
        let mut front = intake.front().await;
        if restored_after_reconnect {
            let old = front.clone();
            intake
                .mailbox
                .v2_request(move |slot| {
                    slot.as_mut().unwrap().hold_front(&old)?;
                    slot.take();
                    Ok(())
                })
                .await
                .unwrap();
            let persistence = discord::queue_persistence_context(
                &intake.shared,
                &ProviderKind::Claude,
                ChannelId::new(CHANNEL),
            );
            intake
                .mailbox
                .v2_ensure_channel(&persistence, ChannelId::new(CHANNEL))
                .await
                .unwrap();
            front = intake
                .mailbox
                .v2_request(|slot| {
                    let owner = slot.as_mut().unwrap();
                    let front = owner.front().unwrap();
                    owner.grant_front(&front)?;
                    Ok(front)
                })
                .await
                .unwrap();
        }
        let before = dispatch::OwnerTurn::probes(CHANNEL);
        let (entered, resume) =
            discord::router::intake_dispatch::after_upload_take(ChannelId::new(CHANNEL))
                .await
                .pause();
        let start =
            dispatch::start_idle_for_test(intake.shared.clone(), ChannelId::new(CHANNEL), front);
        let record = async {
            tokio::time::timeout(LIMIT, entered).await.unwrap().unwrap();
            let guard = test_rest::install(intake._api.http.clone());
            resume.send(()).unwrap();
            guard
        };
        let (result, _rest) = tokio::join!(tokio::time::timeout(LIMIT, start), record);
        let result = result.unwrap();
        eprintln!("MISSING_IDLE_RESULT {result:?}");
        let after = dispatch::OwnerTurn::probes(CHANNEL);
        assert_eq!(
            after.0 - before.0,
            1,
            "the actual idle intake made the early decision"
        );
        let row: Value =
            sqlx::query_scalar("SELECT hosted_execution FROM sessions WHERE channel_id=$1")
                .bind(CHANNEL.to_string())
                .fetch_one(&fx.pool)
                .await
                .unwrap();
        assert_eq!(
            row["state"],
            if missing { "retired" } else { "bound" },
            "{result:?}"
        );
        let state = intake
            .mailbox
            .v2_request(|slot| slot.as_ref().unwrap().json())
            .await
            .unwrap();
        assert!(
            state["items"].as_array().unwrap().is_empty(),
            "idle input stayed queued"
        );
        assert!(
            state["retained_inputs"].as_array().unwrap().is_empty(),
            "refused input was retained"
        );
        assert!(!discord::queue_io::input_owner::held_watch::has_watch(
            CHANNEL
        ));
        assert!(
            !intake
                .shared
                .turn_view_reconciler
                .ops()
                .iter()
                .any(|op| op.add && op.emoji == '📬')
        );
        let calls = intake._api.take();
        assert_eq!(
            calls
                .iter()
                .filter(|call| call.contains("이 입력을 폐기했습니다"))
                .count(),
            1,
            "one channel reason"
        );
        assert_eq!(fx.rig.sends().len(), 2, "no input reached the old pane");
    });
    drop(intake);
    drop(original_runtime);
}

#[test]
fn a_missing_idle_bound_pane_retires_and_a_failed_restart_discards_the_front_pg() {
    idle_case(true, false);
}

#[test]
fn an_unreadable_idle_bound_pane_is_not_retired_and_its_front_is_discarded_pg() {
    idle_case(false, false);
}

#[test]
fn a_front_restored_after_missing_reconnect_uses_the_same_disposal_rule_pg() {
    idle_case(true, true);
}

#[path = "provider_dispatch_herdr_missing_success_tests.rs"]
mod success;
