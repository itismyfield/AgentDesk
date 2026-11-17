//! A committed clear reaches the real Herdr launch script before native binding selection.

use super::*;
use crate::db::dispatched_sessions::provider_resume_fence as fence;
use crate::services::tui_prompt_dedupe::binding_context::PreparedIncarnation;
use crate::services::tui_prompt_dedupe::{self as dedupe, binding_events as events};
use std::os::unix::fs::MetadataExt;

/// A native journal source, verified already or awaiting the retired-resume repin.
fn native_source(fx: &Fixture, verified: bool) -> (String, HostedRecord) {
    let (session, path) = fx.seed_resumable(true);
    let prepared = PreparedIncarnation::prepare(
        "claude",
        fx.logical(),
        Some(CHANNEL),
        Some(&session),
        !verified,
    )
    .unwrap();
    let nonce = &prepared.context.execution_nonce;
    for (extension, text) in [("spawn_nonce", nonce.as_str()), ("host_kind", "herdr")] {
        let marker = crate::services::tmux_common::session_temp_path(fx.logical(), extension);
        std::fs::write(marker, text).unwrap();
    }
    let mut retired = pending_on(&fx.owner, nonce, None, Some(expected(nonce)));
    retired.state = HostedState::Retired;
    fx.store(&retired);
    let metadata = std::fs::metadata(&path).unwrap();
    let now = chrono::Utc::now();
    let event = events::BindingEvent {
        seq: 1,
        channel_id: CHANNEL,
        provider: "claude".into(),
        tmux_session: fx.logical().into(),
        execution_nonce: Some(nonce.clone()),
        old: None,
        new: events::BindingTarget::Source(events::SourceId {
            session_id: session.clone(),
            path,
            dev: metadata.dev(),
            ino: metadata.ino(),
        }),
        cause: if verified {
            events::BindingCause::Startup
        } else {
            events::BindingCause::Resume
        },
        parent_hint: None,
        evidence: events::BindingEvidence {
            hook_event: Some("session_start".into()),
            received_at: now,
        },
        committed_at: now,
    };
    let mut value = serde_json::to_value(event).unwrap();
    value["verified"] = verified.into();
    let log = events::test_root()
        .unwrap()
        .join(events::BINDING_EVENTS_DIR)
        .join(format!("{CHANNEL}.log"));
    std::fs::create_dir_all(log.parent().unwrap()).unwrap();
    std::fs::write(log, format!("{value}\n")).unwrap();
    events::forget_channel_for_tests(CHANNEL);
    crate::services::tmux_common::write_claude_session_auth(
        fx.logical(),
        &session,
        crate::services::provider_auth_profile::DEFAULT_PROFILE_ID,
    )
    .unwrap();
    (session, HostedRecord::Known(retired))
}

fn set_clear(fx: &Fixture, row: &HostedRecord) -> fence::ResumeFence {
    let HostedRecord::Known(record) = row else {
        panic!("native source has an execution");
    };
    fx.rt.block_on(async {
        let id = sqlx::query_scalar("SELECT id FROM sessions WHERE session_key = $1")
            .bind(session_key(&fx.owner))
            .fetch_one(&fx.pool)
            .await
            .unwrap();
        assert!(
            fence::set_for_execution_pg(&fx.pool, id, record)
                .await
                .unwrap()
        );
        fence::load_for_owner_pg(&fx.pool, &fx.owner)
            .await
            .unwrap()
            .unwrap()
    })
}

fn journal_count() -> usize {
    events::binding_events_since(CHANNEL, 0).unwrap().len()
}

/// The assertion observes the production script before its fresh start can append a journal event.
fn fresh_turn(fx: &Fixture, row: &HostedRecord, old: &str, writes_before: usize) -> Started {
    fresh_turn_before_start(fx, row, old, writes_before, || {})
}

fn fresh_turn_before_start(
    fx: &Fixture,
    row: &HostedRecord,
    old: &str,
    writes_before: usize,
    before_start: impl FnOnce(),
) -> Started {
    let before = journal_count();
    let launcher = Arc::new(Launcher::default());
    let mut launched = None;
    let (result, _) = fx.turn(row, &fx.ports(&launcher), || {
        let Some(started) = fx.start_provider(&launcher, false) else {
            return;
        };
        let script = fx.launch_script();
        assert!(
            script.contains(&format!("'--session-id' '{}'", started.session)),
            "{script}"
        );
        assert!(!script.contains("'--resume'"), "{script}");
        assert_ne!(started.session, old);
        assert_eq!(
            journal_count(),
            before,
            "selection must not repin the old source"
        );
        before_start();
        fx.started.store(true, Ordering::SeqCst);
        let payload =
            json!({"session_id":started.session,"source":"startup","transcript_path":started.path});
        assert_eq!(started.hook(fx, "SessionStart", &payload), 202);
        fx.answer_after(&started, writes_before);
        launched = Some(started);
    });
    assert_eq!(result, Ok(()));
    let started = launched.expect("fresh launch reached its verified SessionStart");
    // The provider's Stop verifies its file after the attach resolved the missing-file Pending.
    let payload = json!({"session_id":started.session,"transcript_path":started.path});
    assert_eq!(started.hook(fx, "Stop", &payload), 202);
    started
}

#[test]
fn verified_native_source_with_no_original_starts_fresh_after_clear_pg() {
    let fx = Fixture::new("clear-verified-source", None);
    let (old, row) = native_source(&fx, true);
    set_clear(&fx, &row);
    *fx.session.lock().unwrap() = None;
    fresh_turn(&fx, &row, &old, 0);
}

#[test]
fn retired_resume_source_is_not_repinned_when_clear_requires_fresh_pg() {
    let fx = Fixture::new("clear-unverified-resume", None);
    let (old, row) = native_source(&fx, false);
    set_clear(&fx, &row);
    *fx.session.lock().unwrap() = None;
    fresh_turn(&fx, &row, &old, 0);
}

#[test]
fn forced_old_original_cannot_bypass_the_clear_fence_pg() {
    let fx = Fixture::new("clear-forced-original", None);
    let (old, row) = native_source(&fx, true);
    set_clear(&fx, &row);
    assert_eq!(fx.session.lock().unwrap().as_deref(), Some(old.as_str()));
    fresh_turn(&fx, &row, &old, 0);
}

#[test]
fn a_not_sent_launch_keeps_the_clear_fence_for_fresh_retry_pg() {
    let fx = Fixture::new("clear-not-sent-retry", None);
    let (old, row) = native_source(&fx, false);
    let captured = set_clear(&fx, &row);
    *fx.session.lock().unwrap() = None;
    let launcher = Arc::new(Launcher::default());
    *launcher.create_fails.lock().unwrap() =
        Some(HerdrCreateOutcome::NotSent("not connected".into()));
    let before = journal_count();
    let (first, _) = fx.turn(&row, &fx.ports(&launcher), || {});
    assert!(first.unwrap_err().contains("NotSent"));
    assert!(fx.launch_script().contains("'--session-id'"));
    assert!(!fx.launch_script().contains("'--resume'"));
    assert_eq!(journal_count(), before);
    assert_eq!(
        fx.rt
            .block_on(fence::load_for_owner_pg(&fx.pool, &fx.owner))
            .unwrap(),
        Some(captured)
    );
    fresh_turn(&fx, &fx.record(), &old, 0);
}

#[test]
fn verified_fresh_start_releases_the_fence_and_restart_resumes_the_new_session_pg() {
    let fx = Fixture::new("clear-release-resume", None);
    let (old, row) = native_source(&fx, true);
    set_clear(&fx, &row);
    *fx.session.lock().unwrap() = None;
    let fresh = fresh_turn(&fx, &row, &old, 0);
    assert_eq!(
        fx.rt
            .block_on(fence::load_for_owner_pg(&fx.pool, &fx.owner))
            .unwrap(),
        None
    );
    super::tmux_launch_tests::read_through(&fresh.path, &fresh.session);
    let HostedRecord::Known(mut retired) = fx.record() else {
        panic!("fresh execution");
    };
    let pin = events::claude_history_at(CHANNEL, fx.logical(), Some(&retired.execution_nonce))
        .unwrap()
        .0
        .map(|(source, _)| source.session_id);
    assert_eq!(
        pin.as_deref(),
        Some(fresh.session.as_str()),
        "the provider hook verified the fresh source"
    );
    retired.state = HostedState::Retired;
    fx.store(&retired);
    dedupe::release_herdr_execution(fx.logical(), &retired.execution_nonce);
    events::forget_channel_for_tests(CHANNEL);
    *fx.session.lock().unwrap() = None;
    let launcher = Arc::new(Launcher::default());
    let (result, _) = fx.turn(&HostedRecord::Known(retired), &fx.ports(&launcher), || {
        if let Some(started) = fx.start_provider_as(&launcher, true, "resume") {
            assert_eq!(started.session, fresh.session);
            let script = fx.launch_script();
            assert!(
                script.contains(&format!("'--resume' '{}'", fresh.session)),
                "{script}"
            );
            assert!(!script.contains("'--session-id'"), "{script}");
            fx.answer_after(&started, 2);
        }
    });
    assert_eq!(result, Ok(()));
}

#[test]
fn another_clear_token_survives_the_prior_release_and_forces_fresh_argv_pg() {
    let fx = Fixture::new("clear-replaced-token", None);
    let (old, row) = native_source(&fx, false);
    let first = set_clear(&fx, &row);
    let second = set_clear(&fx, &row);
    assert_ne!(first.token, second.token);
    assert!(!fx.rt.block_on(fence::release_pg(&fx.pool, &first)).unwrap());
    assert_eq!(
        fx.rt
            .block_on(fence::load_for_owner_pg(&fx.pool, &fx.owner))
            .unwrap(),
        Some(second)
    );
    fresh_turn(&fx, &row, &old, 0);
}

#[test]
fn fence_read_failure_refuses_launch_without_native_repin_or_argv_pg() {
    let fx = Fixture::new("clear-unreadable-fence", None);
    let (_, row) = native_source(&fx, false);
    fx.rt.block_on(async {
        sqlx::query("UPDATE sessions SET provider_resume_fence='{}'::jsonb")
            .execute(&fx.pool)
            .await
            .unwrap();
    });
    let launcher = Arc::new(Launcher::default());
    let before = journal_count();
    let (result, _) = fx.turn(&row, &fx.ports(&launcher), || {});
    assert!(result.unwrap_err().contains("provider resume fence"));
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 0);
    assert_eq!(journal_count(), before);
}

#[test]
fn a_created_launch_without_verified_session_start_keeps_the_fence_pg() {
    let fx = Fixture::new("clear-unverified-start", None);
    let (old, row) = native_source(&fx, true);
    let captured = set_clear(&fx, &row);
    let launcher = Arc::new(Launcher::default());
    let (result, _) = fx.turn(&row, &fx.ports(&launcher), || {
        if let Some(started) = fx.start_provider(&launcher, false) {
            assert_ne!(started.session, old);
            assert!(fx.launch_script().contains("'--session-id'"));
            fx.cancel_now();
        }
    });
    assert!(result.unwrap_err().contains("SessionStart"));
    assert_eq!(
        fx.rt
            .block_on(fence::load_for_owner_pg(&fx.pool, &fx.owner))
            .unwrap(),
        Some(captured)
    );
}

#[test]
fn second_clear_before_fresh_verification_survives_release_and_forces_next_launch_fresh_pg() {
    let fx = Fixture::new("clear-during-start", None);
    let (old, row) = native_source(&fx, false);
    let first = set_clear(&fx, &row);
    let mut second = None;
    let fresh = fresh_turn_before_start(&fx, &row, &old, 0, || {
        second = Some(set_clear(&fx, &fx.record()));
    });
    let second = second.unwrap();
    assert_ne!(first.token, second.token);
    assert_eq!(
        fx.rt
            .block_on(fence::load_for_owner_pg(&fx.pool, &fx.owner))
            .unwrap(),
        Some(second)
    );
    let HostedRecord::Known(mut retired) = fx.record() else {
        panic!("fresh row");
    };
    retired.state = HostedState::Retired;
    fx.store(&retired);
    dedupe::release_herdr_execution(fx.logical(), &retired.execution_nonce);
    fresh_turn(&fx, &HostedRecord::Known(retired), &fresh.session, 2);
}

#[test]
fn a_restart_without_clear_resumes_the_verified_native_session_pg() {
    let fx = Fixture::new("restart-without-clear", None);
    let (old, row) = native_source(&fx, true);
    *fx.session.lock().unwrap() = None;
    events::forget_channel_for_tests(CHANNEL);
    let launcher = Arc::new(Launcher::default());
    let (result, _) = fx.turn(&row, &fx.ports(&launcher), || {
        if let Some(started) = fx.start_provider_as(&launcher, true, "resume") {
            assert_eq!(started.session, old);
            let script = fx.launch_script();
            assert!(script.contains(&format!("'--resume' '{old}'")), "{script}");
            assert!(!script.contains("'--session-id'"), "{script}");
            fx.answer(&started);
        }
    });
    assert_eq!(result, Ok(()));
}

#[test]
fn actual_slash_clear_and_restarted_runtime_produce_fresh_launch_argv_pg() {
    let fx = Fixture::new("slash-clear-native-source", None);
    let (old, row) = native_source(&fx, true);
    let channel = poise::serenity_prelude::ChannelId::new(CHANNEL);
    let _hosts = crate::config::session_hosts::force_for_test(Some(NODE), &[(CHANNEL, NODE)]);
    let _registry = fx.rig.registry_on_this_thread();
    fx.rt.block_on(Box::pin(async {
        let mut shared = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
            fx.pool.clone(),
        ));
        Arc::get_mut(&mut shared).unwrap().token_hash = TOKEN.into();
        crate::services::discord::host_defer_gate::tests::map_channel(
            &shared,
            channel,
            fx.logical().strip_prefix("AgentDesk-claude-").unwrap(),
        )
        .await;
        crate::services::discord::commands::slash_clear_for_test(
            shared.clone(),
            crate::services::provider::ProviderKind::Claude,
            channel,
        )
        .await;
        assert!(shared.core.lock().await.sessions[&channel].cleared);
        drop(shared);
        let restarted = crate::services::discord::make_shared_data_for_tests_with_storage(Some(
            fx.pool.clone(),
        ));
        crate::services::discord::host_defer_gate::tests::map_channel(
            &restarted,
            channel,
            "restarted",
        )
        .await;
        assert!(!restarted.core.lock().await.sessions[&channel].cleared);
        assert!(
            fence::load_for_owner_pg(&fx.pool, &fx.owner)
                .await
                .unwrap()
                .is_some()
        );
    }));
    *fx.session.lock().unwrap() = None;
    events::forget_channel_for_tests(CHANNEL);
    fresh_turn(&fx, &row, &old, 0);
}

#[test]
fn held_bound_clear_refuses_old_pane_then_retirement_launches_fresh_pg() {
    let fx = Fixture::new("clear-held-bound", None);
    let first_launcher = Arc::new(Launcher::default());
    let (nonce, old) = super::reconnect::launch(&fx, &first_launcher);
    let bound = fx.record();
    fx.rig.restart_shell(&context_of(&nonce));
    let captured = set_clear(&fx, &bound);
    let launcher = Arc::new(Launcher::default());
    let (result, _) = fx.turn(&bound, &fx.ports(&launcher), || {});
    let refused = result.unwrap_err();
    assert!(
        refused.starts_with("herdr turn: bound execution not confirmed:"),
        "{refused}"
    );
    assert_eq!(fx.record(), bound, "a refused old pane stays unchanged");
    assert_eq!(fx.rig.sends().len(), 2, "no input after the old turn");
    assert_eq!(launcher.creates.load(Ordering::SeqCst), 0);
    assert_eq!(
        fx.rt
            .block_on(fence::load_for_owner_pg(&fx.pool, &fx.owner))
            .unwrap(),
        Some(captured)
    );
    let HostedRecord::Known(mut retired) = bound else {
        panic!("old Bound execution");
    };
    retired.state = HostedState::Retired;
    fx.store(&retired);
    dedupe::release_herdr_execution(fx.logical(), &nonce);
    fresh_turn(&fx, &HostedRecord::Known(retired), &old.session, 2);
}
