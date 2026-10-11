use super::o_seed_install_tests as seed_cases;
use super::*;
use crate::services::discord::health::legacy_supervision::RetiredForTest;
use crate::services::discord::health::legacy_supervision::test_support::tree_fingerprint;
use crate::services::discord::tmux::OOnlyInstallOutcome;

fn child(test: &str) -> bool {
    isolated_in("retired_o_boundary_tests", test, &[])
}

fn monitor_notification(id: &str) -> String {
    let event = serde_json::json!({
        "type": "system", "subtype": "task_notification", "task_id": id,
        "status": "completed", "summary": "Monitor event: boundary probe",
        "task_notification_kind": "monitor_auto_turn"
    });
    format!("{event}\n")
}

fn ready_init(session: &str) -> String {
    let event = serde_json::json!({
        "type": "system", "subtype": "init", "session_id": session
    });
    format!("{event}\n")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p2_i_retired_watcher_rejects_actual_restored_argument_before_seed() {
    let test = "p2_i_retired_watcher_rejects_actual_restored_argument_before_seed";
    if !child(test) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, discord) = seed_cases::fixture(632_512_009).await;
    let mut old_row = seed_cases::seed(&fixture, 0);
    old_row.full_response = "OLD restored argument must never seed O".into();
    old_row.response_sent_offset = 0;
    old_row.injected_prompt_message_id = Some(632_512_900);
    let restored = restored_watcher_turn_from_inflight(&old_row, &fixture.session, true)
        .expect("matching source and real message id create a restored argument");
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    let observed = seed_cases::observe(&fixture);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    seed_cases::until(&observed, "installed predecessor EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    fixture.cancel_and_join().await;
    let predecessor_eof_count = observed.snapshot().event_count("outer_eof");
    assert!(matches!(
        watch_host_of(
            &fixture.shared,
            &fixture.provider,
            fixture.channel.get(),
            &fixture.session,
        )
        .await,
        WatchHost::Legacy
    ));
    let handle = seed_cases::spawn_legacy(&fixture, 0, Some(restored));
    seed_cases::until(&observed, "actual restored-argument successor EOF", |s| {
        s.event_count("outer_eof") > predecessor_eof_count
            && s.event_count("restored_turn_argument_rejected") == 1
    })
    .await;
    let fresh = said("fresh body after rejecting argument");
    seed_cases::append(&fixture, fresh.as_bytes());
    seed_cases::until(&observed, "successor decodes fresh source", |s| {
        s.decoded_chunks.concat() == fresh
    })
    .await;
    seed_cases::cancel_join(&handle).await;
    seed_cases::assert_rowless(&observed);
    let seen = observed.snapshot();
    assert_eq!(seen.event_count("restored_turn_argument_rejected"), 1);
    assert!(
        seen.parser_responses
            .iter()
            .any(|response| response == "fresh body after rejecting argument"),
        "{seen:?}"
    );
    assert!(
        seen.parser_responses
            .iter()
            .all(|response| !response.contains("OLD restored argument")),
        "{seen:?}"
    );
    assert_eq!(tree_fingerprint(&root), before);
    assert!(
        discord
            .requests_for(fixture.channel.get())
            .iter()
            .all(|request| !request.contains("OLD restored argument")),
        "restored anchors and body must not escape through Discord"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p2_j_retired_monitor_initial_and_inner_candidates_do_not_start_legacy_turns() {
    let test = "p2_j_retired_monitor_initial_and_inner_candidates_do_not_start_legacy_turns";
    if !child(test) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, _discord) = seed_cases::fixture(632_512_010).await;
    seed_cases::seed(&fixture, 0);
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    assert!(!crate::services::tui_o::turn_mode::transcript_turns(
        fixture.channel.get()
    ));
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    let observed = seed_cases::observe(&fixture);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    seed_cases::until(&observed, "installed monitor EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let initial = format!(
        "{}{}",
        monitor_notification("initial-monitor"),
        said("initial monitor body")
    );
    seed_cases::append(&fixture, initial.as_bytes());
    seed_cases::until(&observed, "initial monitor candidate and inner EOF", |s| {
        s.event_count("monitor_initial_candidate") > 0 && s.event_count("inner_eof") > 0
    })
    .await;
    let inner = format!(
        "{}{}",
        monitor_notification("inner-monitor"),
        said("inner monitor body")
    );
    seed_cases::append(&fixture, inner.as_bytes());
    seed_cases::until(&observed, "inner monitor candidate", |s| {
        s.event_count("monitor_inner_candidate") > 0
            && s.decoded_chunks.concat() == format!("{initial}{inner}")
    })
    .await;
    fixture.cancel_and_join().await;
    seed_cases::assert_rowless(&observed);
    let seen = observed.snapshot();
    assert!(
        seen.event_count("monitor_initial_candidate") > 0,
        "{seen:?}"
    );
    assert!(seen.event_count("monitor_inner_candidate") > 0, "{seen:?}");
    assert_eq!(seen.event_count("monitor_start_call"), 0, "{seen:?}");
    assert_eq!(seen.event_count("monitor_upsert_call"), 0, "{seen:?}");
    assert_eq!(tree_fingerprint(&root), before);
    assert!(fixture.shared.core.lock().await.sessions.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p2_h_actual_fresh_idle_preserves_partial_body_until_provider_parse() {
    let test = "p2_h_actual_fresh_idle_preserves_partial_body_until_provider_parse";
    if !child(test) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, _discord) = seed_cases::fixture(632_512_008).await;
    seed_cases::seed(&fixture, 0);
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    let coord = fixture.shared.tmux_relay_coord(fixture.channel);
    let frontier = coord.confirmed_end_offset.load(Ordering::Acquire);
    let observed = seed_cases::observe(&fixture);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    seed_cases::until(&observed, "fresh-idle initial EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let init = ready_init("fresh-idle-session");
    let body = "fresh carry body survives idle";
    // The missing top-level type keeps this unfinished body unclassified until its closing read.
    let message =
        serde_json::json!({"role": "assistant", "content": [{"type": "text", "text": body}]});
    let partial = format!("{init}{{\"message\":{message},");
    let closing = "\"type\":\"assistant\"}\n";
    seed_cases::append(&fixture, partial.as_bytes());
    seed_cases::until(
        &observed,
        "actual body bytes decoded before classification",
        |s| s.decoded_chunks.concat() == partial && s.event_count("decoder_initialized") > 0,
    )
    .await;
    let initial_decoder_count = observed.snapshot().event_count("decoder_initialized");
    seed_cases::until(&observed, "actual parser classifies fresh idle", |s| {
        s.event_count("fresh_idle_classified") == 1 && s.event_count("fresh_idle_no_result") == 1
    })
    .await;
    seed_cases::until(
        &observed,
        "fresh-idle handler preserves carry into next collector",
        |s| s.event_count("decoder_initialized") > initial_decoder_count,
    )
    .await;
    assert!(
        observed
            .snapshot()
            .parser_responses
            .iter()
            .all(String::is_empty)
    );
    seed_cases::append(&fixture, closing.as_bytes());
    seed_cases::until(
        &observed,
        "closing source read reaches actual provider parser",
        |s| s.decoded_chunks.concat() == format!("{partial}{closing}"),
    )
    .await;
    fixture.cancel_and_join().await;
    seed_cases::assert_rowless(&observed);
    let seen = observed.snapshot();
    assert_eq!(seen.decoded_chunks.concat(), format!("{partial}{closing}"));
    assert!(
        seen.parser_responses
            .iter()
            .any(|response| response == body),
        "{seen:?}"
    );
    assert_eq!(seen.event_count("fresh_idle_classified"), 1, "{seen:?}");
    assert_eq!(seen.event_count("post_work_idle_classified"), 0, "{seen:?}");
    assert_eq!(
        seen.event_count("fresh_idle_legacy_settlement"),
        0,
        "{seen:?}"
    );
    assert_eq!(coord.confirmed_end_offset.load(Ordering::Acquire), frontier);
    assert_eq!(tree_fingerprint(&root), before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn p2_h_actual_post_work_idle_keeps_parsed_body_rowless_until_cancel() {
    let test = "p2_h_actual_post_work_idle_keeps_parsed_body_rowless_until_cancel";
    if !child(test) {
        return;
    }
    let _env_lock = crate::config::test_env_lock::acquire_shared_test_env_lock();
    let (fixture, _discord) = seed_cases::fixture(632_512_011).await;
    seed_cases::seed(&fixture, 0);
    let _retired = RetiredForTest::new("claude", fixture.channel.get());
    let root = crate::services::discord::runtime_store::discord_inflight_root().unwrap();
    let before = tree_fingerprint(&root);
    let coord = fixture.shared.tmux_relay_coord(fixture.channel);
    let frontier = coord.confirmed_end_offset.load(Ordering::Acquire);
    let observed = seed_cases::observe(&fixture);
    assert_eq!(fixture.install().await, OOnlyInstallOutcome::Spawned);
    let handle = fixture.handle();
    seed_cases::until(&observed, "post-work initial EOF", |s| {
        s.event_count("outer_eof") > 0
    })
    .await;
    let body = "Parsed BODY remains through idle.";
    let initial = format!("{}{}", said(body), ready_init("post-work-idle-session"));
    seed_cases::append(&fixture, initial.as_bytes());
    seed_cases::until(&observed, "actual post-work body decode", |s| {
        s.decoded_chunks.concat() == initial
            && s.event_count("inner_eof") > 0
            && s.parser_responses.iter().any(|response| response == body)
    })
    .await;
    seed_cases::until(&observed, "actual post-work idle classification", |s| {
        s.event_count("post_work_idle_classified") > 0
    })
    .await;

    let tail = " New source BODY after the idle.";
    let suffix = said(tail);
    seed_cases::append(&fixture, suffix.as_bytes());
    seed_cases::until(&observed, "source read after actual post-work idle", |s| {
        s.decoded_chunks.concat() == format!("{initial}{suffix}")
            || s.writable_inflight_load_calls > 0
            || s.readonly_inflight_load_calls > 0
    })
    .await;
    seed_cases::cancel_join(&handle).await;
    seed_cases::assert_rowless(&observed);
    let seen = observed.snapshot();
    assert!(
        seen.event_count("post_work_idle_classified") > 0,
        "{seen:?}"
    );
    assert_eq!(seen.event_count("fresh_idle_classified"), 0, "{seen:?}");
    assert_eq!(
        seen.event_count("fresh_idle_legacy_settlement"),
        0,
        "{seen:?}"
    );
    assert_eq!(seen.decoded_chunks.concat(), format!("{initial}{suffix}"));
    let retained = seen
        .parser_responses
        .last()
        .expect("joined actual parser checkpoint");
    assert!(
        retained.contains(body) && retained.contains(tail),
        "{seen:?}"
    );
    assert!(
        retained.find(body).unwrap() < retained.find(tail).unwrap(),
        "{seen:?}"
    );
    assert_eq!(coord.confirmed_end_offset.load(Ordering::Acquire), frontier);
    assert_eq!(tree_fingerprint(&root), before);
}
